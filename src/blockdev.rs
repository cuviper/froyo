// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::io::{Read, Write, ErrorKind, Seek, SeekFrom};
use std::fs::{OpenOptions, read_dir};
use std::path::{Path, PathBuf};
use std::io;
use std::rc::Rc;
use std::cell::RefCell;
use std::str::{FromStr, from_utf8};
use std::cmp::Ordering;

use nix::sys::stat;
use time::Timespec;
use devicemapper::{DM, Device};
use crc::crc32;
use byteorder::{LittleEndian, ByteOrder};
use uuid::Uuid;


use types::{Sectors, SectorOffset, FroyoError};
use consts::*;
use util::{setup_dm_dev, blkdev_size};



#[derive(Debug, Clone, PartialEq)]
pub struct MDA {
    pub last_updated: Timespec,
    length: u32,
    crc: u32,
    offset: SectorOffset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDevSave {
    pub path: PathBuf,
    pub sectors: Sectors,
}

#[derive(Debug, Clone)]
pub struct BlockDev {
    pub froyodev_id: String,
    dev: Device,
    pub id: String,
    pub path: PathBuf,
    sectors: Sectors,
    pub mdaa: MDA,
    pub mdab: MDA,
    pub linear_devs: Vec<Rc<RefCell<LinearDev>>>,
}

impl BlockDev {
    pub fn new(path: &Path) -> io::Result<BlockDev> {
        let dev = try!(Device::from_str(&path.to_string_lossy()));

        let mut f = match OpenOptions::new().read(true).open(path) {
            Err(_) => {
                return Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    format!("Could not open {}", path.display())));
            },
            Ok(x) => x,
        };

        let mut buf = [0u8; HEADER_SIZE as usize];
        try!(f.read(&mut buf));

        if &buf[4..20] != FRO_MAGIC {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{} is not a Froyo device", path.display())));
        }

        let crc = crc32::checksum_ieee(&buf[4..HEADER_SIZE as usize]);
        if crc != LittleEndian::read_u32(&mut buf[..4]) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{} Froyo header CRC failed", path.display())));
            // TODO: Try to read end-of-disk copy
        }

        let sectors = Sectors::new(try!(blkdev_size(&f)) / SECTOR_SIZE);

        let id = from_utf8(&buf[32..64]).unwrap();
        let froyodev_id = from_utf8(&buf[128..160]).unwrap();

        Ok(BlockDev {
            froyodev_id: froyodev_id.to_owned(),
            id: id.to_owned(),
            dev: dev,
            path: path.to_owned(),
            sectors: sectors,
            mdaa: MDA {
                last_updated: Timespec::new(
                    LittleEndian::read_u64(&buf[64..72]) as i64,
                    LittleEndian::read_u32(&buf[72..76]) as i32),
                length: LittleEndian::read_u32(&buf[76..80]),
                crc: LittleEndian::read_u32(&buf[80..84]),
                offset: MDAA_ZONE_OFFSET,
            },
            mdab: MDA {
                last_updated: Timespec::new(
                    LittleEndian::read_u64(&buf[96..104]) as i64,
                    LittleEndian::read_u32(&buf[104..108]) as i32),
                length: LittleEndian::read_u32(&buf[108..112]),
                crc: LittleEndian::read_u32(&buf[112..116]),
                offset: MDAB_ZONE_OFFSET,
            },
            linear_devs: Vec::new(), // Not initialized until metadata is read
        })
    }

    pub fn initialize(froyodev_id: &str, path: &Path, force: bool) -> io::Result<BlockDev> {
        let pstat = match stat::stat(path) {
            Err(_) => return Err(io::Error::new(
                ErrorKind::NotFound,
                format!("{} not found", path.display()))),
            Ok(x) => x,
        };

        if pstat.st_mode & 0x6000 != 0x6000 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{} is not a block device", path.display())));
        }

        let dev = match Device::from_str(&path.to_string_lossy()) {
            Err(_) => return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{} is not a block device", path.display()))),
            Ok(x) => x,
        };

        let mut f = match OpenOptions::new().read(true).write(true).open(path) {
            Err(_) => return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("Could not open {}", path.display()))),
            Ok(x) => x,
        };

        if !force {
            let mut buf = [0u8; 4096];
            try!(f.read(&mut buf));

            if buf.iter().any(|x| *x != 0) {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("First 4K of {} is not zeroed, need to use --force",
                            path.display())));
            }
        }

        let dev_size = try!(blkdev_size(&f));
        if dev_size < MIN_DEV_SIZE {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{} too small, 1G minimum", path.display())));
        }

        let mut bd = BlockDev {
            froyodev_id: froyodev_id.to_owned(),
            id: Uuid::new_v4().to_simple_string(),
            dev: dev,
            path: path.to_owned(),
            sectors: Sectors::new(dev_size / SECTOR_SIZE),
            mdaa: MDA {
                last_updated: Timespec::new(0,0),
                length: 0,
                crc: 0,
                offset: MDAA_ZONE_OFFSET,
            },
            mdab: MDA {
                last_updated: Timespec::new(0,0),
                length: 0,
                crc: 0,
                offset: MDAB_ZONE_OFFSET,
            },
            linear_devs: Vec::new(),
        };

        try!(bd.write_mda_header());

        Ok(bd)
    }

    pub fn to_save(&self) -> BlockDevSave {
        BlockDevSave {
            path: self.path.clone(),
            sectors: self.sectors,
        }
    }

    pub fn find_all() -> Result<Vec<BlockDev>, FroyoError> {
        Ok(try!(read_dir("/dev"))
            .into_iter()
            .filter_map(|dir_e| if dir_e.is_ok()
                        { Some(dir_e.unwrap().path()) } else { None } )
            .filter_map(|path| { BlockDev::new(&path).ok() })
            .collect::<Vec<_>>())
    }

    fn used_areas(&self) -> Vec<(SectorOffset, Sectors)> {
        let mut used = Vec::new();

        // Flag start and end mda zones as used
        used.push((SectorOffset::new(0), MDA_ZONE_SECTORS));
        used.push((SectorOffset::new(*self.sectors - *MDA_ZONE_SECTORS), MDA_ZONE_SECTORS));

        for dev in &self.linear_devs {
            let dev = RefCell::borrow(dev);
            for seg in &dev.meta_segments {
                used.push((seg.start, seg.length));
            }
            for seg in &dev.data_segments {
                used.push((seg.start, seg.length));
            }
        }
        used.sort();

        used
    }

    fn free_areas(&self) -> Vec<(SectorOffset, Sectors)> {
        let mut free = Vec::new();

        // Insert an entry to mark the end so the fold works correctly
        let mut used = self.used_areas();
        used.push((SectorOffset::new(*self.sectors), Sectors::new(0)));

        used.into_iter()
            .fold(SectorOffset::new(0), |prev_end, (start, len)| {
                if prev_end < start {
                    free.push((prev_end, Sectors::new(*start - *prev_end)))
                }
                SectorOffset::new(*start + *len)
            });

        free
    }

    pub fn largest_free_area(&self) -> Option<(SectorOffset, Sectors)> {
        self.free_areas().into_iter()
            .max_by_key(|&(_, len)| len)
    }

    // Read metadata from newest MDA
    pub fn read_mdax(&self) -> io::Result<Vec<u8>> {
        let younger_mda = match self.mdaa.last_updated.cmp(&self.mdab.last_updated) {
            Ordering::Less => &self.mdab,
            Ordering::Greater => &self.mdaa,
            Ordering::Equal => &self.mdab,
        };

        if younger_mda.last_updated == Timespec::new(0,0) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput, "Neither MDA region is in use"))
        }

        let mut f = try!(OpenOptions::new().read(true).open(&self.path));
        let mut buf = vec![0; younger_mda.length as usize];

        // read metadata from disk
        try!(f.seek(SeekFrom::Start(*younger_mda.offset * SECTOR_SIZE)));
        try!(f.read_exact(&mut buf));

        if younger_mda.crc != crc32::checksum_ieee(&buf) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput, "Froyo MDA CRC failed"))
                // TODO: Read backup copy
        }

        Ok(buf)
    }

    // Write metadata to least-recently-written MDA
    fn write_mdax(&mut self, time: &Timespec, metadata: &[u8]) -> io::Result<()> {
        let older_mda = match self.mdaa.last_updated.cmp(&self.mdab.last_updated) {
            Ordering::Less => &mut self.mdaa,
            Ordering::Greater => &mut self.mdab,
            Ordering::Equal => &mut self.mdaa,
        };

        if metadata.len() as u64 > *MDAX_ZONE_SECTORS * SECTOR_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Metadata too large for MDA, {} bytes", metadata.len())))
        }

        older_mda.crc = crc32::checksum_ieee(&metadata);
        older_mda.length = metadata.len() as u32;
        older_mda.last_updated = *time;

        let mut f = try!(OpenOptions::new().write(true).open(&self.path));

        // write metadata to disk
        try!(f.seek(SeekFrom::Start(*older_mda.offset * SECTOR_SIZE)));
        try!(f.write_all(&metadata));
        try!(f.seek(SeekFrom::End(-(MDA_ZONE_SIZE as i64))));
        try!(f.seek(SeekFrom::Current((*older_mda.offset * SECTOR_SIZE) as i64)));
        try!(f.write_all(&metadata));
        try!(f.flush());

        Ok(())
    }

    fn write_mda_header(&mut self) -> io::Result<()> {
        let mut buf = [0u8; HEADER_SIZE as usize];
        buf[4..20].clone_from_slice(FRO_MAGIC);
        LittleEndian::write_u64(&mut buf[20..28], *self.sectors);
        // no flags
        buf[32..64].clone_from_slice(self.id.as_bytes());

        LittleEndian::write_u64(&mut buf[64..72], self.mdaa.last_updated.sec as u64);
        LittleEndian::write_u32(&mut buf[72..76], self.mdaa.last_updated.nsec as u32);
        LittleEndian::write_u32(&mut buf[76..80], self.mdaa.length);
        LittleEndian::write_u32(&mut buf[80..84], self.mdaa.crc);

        LittleEndian::write_u64(&mut buf[96..104], self.mdab.last_updated.sec as u64);
        LittleEndian::write_u32(&mut buf[104..108], self.mdab.last_updated.nsec as u32);
        LittleEndian::write_u32(&mut buf[108..112], self.mdab.length);
        LittleEndian::write_u32(&mut buf[112..116], self.mdab.crc);

        buf[128..160].clone_from_slice(self.froyodev_id.as_bytes());

        // All done, calc CRC and write
        let hdr_crc = crc32::checksum_ieee(&buf[4..HEADER_SIZE as usize]);
        LittleEndian::write_u32(&mut buf[..4], hdr_crc);

        let mut f = try!(OpenOptions::new().write(true).open(&self.path));

        try!(f.seek(SeekFrom::Start(0)));
        try!(f.write_all(&buf));
        try!(f.seek(SeekFrom::End(-(MDA_ZONE_SIZE as i64))));
        try!(f.write_all(&buf));
        try!(f.flush());

        Ok(())
    }

    pub fn save_state(&mut self, time: &Timespec, metadata: &[u8]) -> io::Result<()> {
        try!(self.write_mdax(time, metadata));
        try!(self.write_mda_header());

        Ok(())
    }
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearSegment {
    pub start: SectorOffset,
    pub length: Sectors,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearDevSave {
    pub meta_segments: Vec<LinearSegment>,
    pub data_segments: Vec<LinearSegment>,
    pub parent: String,
}

#[derive(Debug, Clone)]
pub struct LinearDev {
    pub meta_dev: Device,
    meta_segments: Vec<LinearSegment>,
    pub data_dev: Device,
    data_segments: Vec<LinearSegment>,
    parent: Rc<RefCell<BlockDev>>,
}

impl LinearDev {
    pub fn create(
        dm: &DM,
        name: &str,
        blockdev: &Rc<RefCell<BlockDev>>,
        meta_segments: &[LinearSegment],
        data_segments: &[LinearSegment])
        -> io::Result<LinearDev> {

        let dev = RefCell::borrow(blockdev).dev;

        // meta
        let mut table = Vec::new();
        let mut offset = SectorOffset::new(0);
        for seg in meta_segments {
            let line = (*offset, *seg.length, "linear",
                        format!("{}:{} {}", dev.major, dev.minor, *seg.start));
            table.push(line);
            offset = offset + SectorOffset::new(*seg.length);
        }

        let dm_name = format!("froyo-linear-meta-{}", name);
        let meta_dev = try!(setup_dm_dev(dm, &dm_name, &table));

        // data
        let mut table = Vec::new();
        let mut offset = SectorOffset::new(0);
        for seg in data_segments {
            let line = (*offset, *seg.length, "linear",
                        format!("{}:{} {}", dev.major, dev.minor, *seg.start));
            table.push(line);
            offset = offset + SectorOffset::new(*seg.length);
        }

        let dm_name = format!("froyo-linear-data-{}", name);
        let data_dev = try!(setup_dm_dev(dm, &dm_name, &table));

        Ok(LinearDev{
            meta_dev: meta_dev,
            meta_segments: meta_segments.to_vec(),
            data_dev: data_dev,
            data_segments: data_segments.to_vec(),
            parent: blockdev.clone(),
        })
    }

    pub fn to_save(&self) -> LinearDevSave {
        LinearDevSave {
            meta_segments: self.meta_segments.clone(),
            data_segments: self.data_segments.clone(),
            parent: RefCell::borrow(&self.parent).id.clone(),
        }
    }

    pub fn data_length(&self) -> Sectors {
        self.data_segments.iter().map(|x| x.length).sum()
    }
}