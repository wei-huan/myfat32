// 对 DBR, FSInfo的抽象,文件系统重要信息管理,以及对文件系统的全局管理.
use super::BlockDevice;
use crate::directory_entry::{ShortDirectoryEntry, DIRENT_SZ};
use crate::error::Error;
use crate::{MAX_CLUS_SZ, START_CLUS_ID};
use spin::rwlock::RwLock;
use std::sync::Arc;

// 并不是 BPB 里面全部的信息,有些过时或者不重要的成员没有在里面
#[derive(Copy, Clone)]
pub(crate) struct BiosParameterBlock {
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    reserved_sectors: u16,
    fats_number: u8,         // FAT 表数,正常的为1或2
    root_entries: u16,       // 根目录的目录项数, FAT32 一直设为0
    total_sectors_16: u16,   // FAT32 固定为0
    media: u8,               // 存储介质类型
    sectors_per_fat_16: u16, // FAT32 固定为0
    sectors_per_track: u16,
    heads: u16,          // 磁头数
    hidden_sectors: u32, // 文件系统前的隐藏扇区数,对于有分区的磁盘来说不为0
    total_sectors_32: u32,
    // Extended BIOS Parameter Block
    fats_sectors: u32,
    extended_flags: u16,
    fs_version: u16,
    root_dir_cluster: u32,
    fsinfo_sector_number: u16,
    backup_boot_sector: u16,
    dummy2: [u8; 15], // 不关心的数据
    volumn_id: u32,
    volume_label: [u8; 11], // 卷名, 11bytes
    fs_type_label: [u8; 8], // 文件系统类型名, 如果是FAT32就是FAT32的ascii码
}

impl BiosParameterBlock {
    // runfat 最先判断是否是 FAT32 类型文件系统
    fn validate_fat32(&self) -> Result<(), Error> {
        if self.root_entries != 0
            || self.total_sectors_16 != 0
            || self.sectors_per_fat_16 != 0
            || self.fs_version != 0
            || std::str::from_utf8(&self.volume_label[0..8]).unwrap() != "FAT32   "
        {
            println!("Unsupported filesystem: Not FAT32");
            return Err(Error::CorruptedFileSystem);
        }
        Ok(())
    }
    // 本项目实现的 FAT32 文件系统扇区的字节数只支持范围在512-4096字节中二的整指数倍
    fn validate_bytes_per_sector(&self) -> Result<(), Error> {
        if self.bytes_per_sector.count_ones() != 1 {
            println!(
                "invalid bytes_per_sector value in BPB: expected a power of two but got {}",
                self.bytes_per_sector
            );
            return Err(Error::CorruptedFileSystem);
        }
        if self.bytes_per_sector < 512 || self.bytes_per_sector > 4096 {
            println!(
                "invalid bytes_per_sector value in BPB: expected value in range [512, 4096] but got {}",
                self.bytes_per_sector
            );
            return Err(Error::CorruptedFileSystem);
        }
        Ok(())
    }
    // 本项目实现的 FAT32 文件系统簇的扇区数只支持二的整指数倍
    fn validate_sectors_per_cluster(&self) -> Result<(), Error> {
        if self.sectors_per_cluster.count_ones() != 1 {
            println!(
                "invalid sectors_per_cluster value in BPB: expected a power of two but got {}",
                self.sectors_per_cluster
            );
            return Err(Error::CorruptedFileSystem);
        }
        if self.sectors_per_cluster < 1 || self.sectors_per_cluster > 128 {
            println!(
                "invalid sectors_per_cluster value in BPB: expected value in range [1, 128] but got {}",
                self.sectors_per_cluster
            );
            return Err(Error::CorruptedFileSystem);
        }
        Ok(())
    }
    // runfat 实现的 FAT32 文件系统簇的字节数必须小于32KB
    fn validate_bytes_per_cluster(&self) -> Result<(), Error> {
        let bytes_per_cluster: usize = usize::from(self.sectors_per_cluster) * usize::from(self.bytes_per_sector);
        if bytes_per_cluster > MAX_CLUS_SZ {
            println!(
                "invalid bytes_per_cluster value in BPB: expected value smaller than {} but got {}",
                MAX_CLUS_SZ, bytes_per_cluster
            );
            return Err(Error::CorruptedFileSystem);
        }
        Ok(())
    }
    fn validate_reserved_sectors(&self) -> Result<(), Error> {
        if self.reserved_sectors < 1 {
            println!(
                "invalid reserved_sectors value in BPB: {}",
                self.reserved_sectors
            );
            return Err(Error::CorruptedFileSystem);
        }
        if self.backup_boot_sector >= self.reserved_sectors {
            println!(
                "Invalid BPB: expected backup boot-sector to be in the reserved region (sector < {}) but got sector {}",
                self.reserved_sectors, self.backup_boot_sector
            );
            return Err(Error::CorruptedFileSystem);
        }
        if self.fsinfo_sector_number >= self.reserved_sectors {
            println!(
                "Invalid BPB: expected FSInfo sector to be in the reserved region (sector < {}) but got sector {}",
                self.reserved_sectors, self.fsinfo_sector_number
            );
            return Err(Error::CorruptedFileSystem);
        }
        Ok(())
    }
    // runfat 实现的 FAT32 文件系统 FAT 表数必须为1或2
    fn validate_fats(&self) -> Result<(), Error> {
        if self.fats_number == 0 || self.fats_number > 2 {
            println!("invalid fats value in BPB: {}", self.fats_number);
            return Err(Error::CorruptedFileSystem);
        }
        Ok(())
    }
    // runfat 实现的 FAT32 文件系统 FAT 表数必须为1或2
    fn validate_root_entries(&self) -> Result<(), Error> {
        if self.fats_number == 0 || self.fats_number > 2 {
            println!("invalid fats value in BPB: {}", self.fats_number);
            return Err(Error::CorruptedFileSystem);
        }
        Ok(())
    }
    fn validate_total_sectors(&self) -> Result<(), Error> {
        let total_sectors = self.total_sectors_32();
        let first_data_sector = self.first_data_sector();
        if self.total_sectors_32 == 0{
            println!("Invalid BPB (total_sectors_32 should be non-zero)");
            return Err(Error::CorruptedFileSystem);
        }
        if total_sectors <= first_data_sector {
            println!(
                "Invalid total_sectors value in BPB: expected value > {} but got {}",
                first_data_sector, total_sectors
            );
            return Err(Error::CorruptedFileSystem);
        }
        Ok(())
    }
    fn validate_fats_sectors(&self) -> Result<(), Error> {
        if self.fats_sectors == 0 {
            println!(
                "Invalid sectors_per_fat_32 value in FAT32 BPB: expected non-zero value but got {}",
                self.fats_sectors
            );
            return Err(Error::CorruptedFileSystem);
        }
        Ok(())
    }
    fn validate_total_clusters(&self) -> Result<(), Error> {
        const FAT32_MAX_CLUSTERS: u32 = 0x0FFF_FFF4;
        let total_clusters = self.total_clusters();
        if total_clusters > FAT32_MAX_CLUSTERS {
            println!("Invalid BPB: too many clusters {}", total_clusters);
            return Err(Error::CorruptedFileSystem);
        }
        let total_fat_entries = self.fats_sectors() * u32::from(self.bytes_per_sector) * 8 / DIRENT_SZ;
        let usable_fat_entries: u32 = total_fat_entries - u32::try_from(START_CLUS_ID).unwrap();
        if usable_fat_entries < total_clusters {
            println!(
                "FAT is too small (allows allocation of {} clusters) compared to the total number of clusters ({})",
                usable_fat_entries, total_clusters
            );
        }
        Ok(())
    }
    // 验证文件系统是否是合法的FAT32类型
    pub(crate) fn validate(&self) -> Result<(), Error> {
        self.validate_fat32()?;
        self.validate_bytes_per_sector()?;
        self.validate_sectors_per_cluster()?;
        self.validate_bytes_per_cluster()?;
        self.validate_reserved_sectors()?;
        self.validate_fats()?;
        self.validate_root_entries()?;
        self.validate_total_sectors()?;
        self.validate_fats_sectors()?;
        self.validate_total_clusters()?;
        Ok(())
    }
    pub(crate) fn fats_sectors(&self) -> u32 {
        self.fats_sectors
    }
    pub(crate) fn total_sectors_32(&self) -> u32 {
        self.total_sectors_32
    }
    pub(crate) fn reserved_sectors(&self) -> u32 {
        u32::from(self.reserved_sectors)
    }
    pub(crate) fn root_dir_sectors(&self) -> u32 {
        let root_dir_bytes = u32::from(self.root_entries) * DIRENT_SZ;
        (root_dir_bytes + u32::from(self.bytes_per_sector) - 1) / u32::from(self.bytes_per_sector)
    }
    pub(crate) fn sectors_per_all_fats(&self) -> u32 {
        u32::from(self.fats_number) * self.fats_sectors()
    }
    pub(crate) fn first_data_sector(&self) -> u32 {
        let root_dir_sectors = self.root_dir_sectors();
        let fat_sectors = self.sectors_per_all_fats();
        self.reserved_sectors() + fat_sectors + root_dir_sectors
    }
    pub(crate) fn total_clusters(&self) -> u32 {
        let total_sectors = self.total_sectors_32();
        let first_data_sector = self.first_data_sector();
        let data_sectors = total_sectors - first_data_sector;
        data_sectors / u32::from(self.sectors_per_cluster)
    }
}

// 本文件系统实现不会改变这个起始扇区,也不能改变起始扇区,因为不具备创建文件系统,扩容等功能
#[derive(Copy, Clone)]
pub(crate) struct BootSector {
    bootjmp: [u8; 3],
    oem_name: [u8; 8],
    bpb: BiosParameterBlock,
    boot_code: [u8; 448],
    boot_sig: [u8; 2],
}

impl BootSector {
    fn initialize() {}
}

pub(crate) struct FSInfo {
    free_cluster_count: u32,
    next_free_cluster: u32,
}

impl FSInfo {
    const LEAD_SIGNATURE: u32 = 0x4161_5252;
    const STRUC_SIGNATURE: u32 = 0x6141_7272;
    const TRAIL_SIGNATURE: u32 = 0xAA55_0000;
}

// 包括 BPB 和 FSInfo 的信息
pub struct RunFileSystem {
    bpb: BiosParameterBlock,
    fsinfo: FSInfo,
    block_device: Arc<dyn BlockDevice>,
    root_dir: Arc<RwLock<ShortDirectoryEntry>>, // 根目录项
}
