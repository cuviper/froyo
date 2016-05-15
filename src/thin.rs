// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::io;
use std::process::Command;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::cell::RefCell;

use devicemapper::{DM, Device, DmFlags, DevId, DM_SUSPEND};
use uuid::Uuid;
use nix::sys::stat::{mknod, umask, Mode, S_IFBLK, S_IRUSR, S_IWUSR, S_IRGRP, S_IWGRP};
use nix::errno::EEXIST;

use types::{Sectors, DataBlocks, FroyoError, FroyoResult, InternalError};
use raid::{RaidSegment, RaidLinearDev, RaidLinearDevSave};
use dmdevice::DmDevice;
use consts::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinPoolDevSave {
    pub data_block_size: Sectors,
    pub low_water_blocks: DataBlocks,
    pub meta_dev: RaidLinearDevSave,
    pub data_dev: RaidLinearDevSave,
}

#[derive(Debug, Clone)]
pub struct ThinPoolDev {
    dev: DmDevice,
    data_block_size: Sectors,
    pub low_water_blocks: DataBlocks,
    params: String,
    pub meta_dev: Rc<RefCell<RaidLinearDev>>,
    pub data_dev: Rc<RefCell<RaidLinearDev>>,
}

#[derive(Debug, Clone, Copy)]
pub struct ThinPoolBlockUsage {
    pub used_meta: u64,
    pub total_meta: u64,
    pub used_data: DataBlocks,
    pub total_data: DataBlocks,
}

#[derive(Debug, Clone, Copy)]
pub enum ThinPoolStatus {
    Good((ThinPoolWorkingStatus, ThinPoolBlockUsage)),
    Fail,
}

#[derive(Debug, Clone, Copy)]
pub enum ThinPoolWorkingStatus {
    Good,
    ReadOnly,
    OutOfSpace,
    NeedsCheck,
}

impl ThinPoolDev {
    pub fn new(dm: &DM,
               id: &str,
               meta_segs: Vec<RaidSegment>,
               data_segs: Vec<RaidSegment>)
               -> FroyoResult<ThinPoolDev> {
        // meta
        let meta_name = format!("thin-meta-{}", id);
        let meta_raid_dev = try!(RaidLinearDev::new(
            dm,
            &meta_name,
            &Uuid::new_v4().to_simple_string(),
            meta_segs));

        try!(meta_raid_dev.dev.clear());

        // data
        let data_name = format!("thin-data-{}", id);
        let data_raid_dev = try!(RaidLinearDev::new(
            dm,
            &data_name,
            &Uuid::new_v4().to_simple_string(),
            data_segs));

        ThinPoolDev::setup(
            dm,
            id,
            DATA_BLOCK_SIZE,
            DataBlocks(TPOOL_LOW_WATER_BLOCKS),
            meta_raid_dev,
            data_raid_dev)
    }

    pub fn setup(
        dm: &DM,
        id: &str,
        data_block_size: Sectors,
        low_water_blocks: DataBlocks,
        meta_raid_dev: RaidLinearDev,
        data_raid_dev: RaidLinearDev)
        -> FroyoResult<ThinPoolDev> {

        let params = format!("{} {} {} {} 1 skip_block_zeroing",
                             meta_raid_dev.dev.dstr(),
                             data_raid_dev.dev.dstr(),
                             *data_block_size,
                             *low_water_blocks);
        let table = [(0u64, *data_raid_dev.length(), "thin-pool", &*params)];

        let dm_name = format!("froyo-thin-pool-{}", id);
        let pool_dev = try!(DmDevice::new(dm, &dm_name, &table));

        let tpool = ThinPoolDev {
            dev: pool_dev,
            data_block_size: data_block_size,
            low_water_blocks: low_water_blocks,
            params: params.clone(),
            meta_dev: Rc::new(RefCell::new(meta_raid_dev)),
            data_dev: Rc::new(RefCell::new(data_raid_dev)),
        };

        // TODO: if needs_check, run the check
        match try!(tpool.status()) {
            ThinPoolStatus::Good((ThinPoolWorkingStatus::Good, _)) => {}
            ThinPoolStatus::Good((ThinPoolWorkingStatus::NeedsCheck, _)) =>
                return Err(FroyoError::Froyo(InternalError(
                    "Froyodev thin pool needs a check".into()))),
            bad => return Err(FroyoError::Froyo(InternalError(
                format!("Froyodev has a failed thin pool: {:?}", bad).into())))
        }

        Ok(tpool)
    }

    pub fn teardown(&mut self, dm: &DM) -> FroyoResult<()> {
        try!(self.dev.teardown(dm));
        try!(self.meta_dev.borrow_mut().teardown(dm));
        try!(self.data_dev.borrow_mut().teardown(dm));

        Ok(())
    }

    pub fn to_save(&self) -> ThinPoolDevSave {
        ThinPoolDevSave {
            data_block_size: self.data_block_size,
            low_water_blocks: self.low_water_blocks,
            meta_dev: self.meta_dev.borrow().to_save(),
            data_dev: self.data_dev.borrow().to_save(),
        }
    }

    pub fn status(&self) -> FroyoResult<ThinPoolStatus> {
        let dm = try!(DM::new());

        let mut status = try!(self.dev.table_status(&dm));

        if status.len() != 1 {
            return Err(FroyoError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected 1 line from thin pool status")))
        }

        let status_line = status.pop().unwrap().3;
        if status_line.starts_with("Fail") {
            return Ok(ThinPoolStatus::Fail)
        }

        let status_vals = status_line.split(' ').collect::<Vec<_>>();
        if status_vals.len() < 8 {
            return Err(FroyoError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Kernel returned too few values from thin pool status")))
        }

        let usage = {
            let meta_vals = status_vals[1].split('/').collect::<Vec<_>>();
            let data_vals = status_vals[2].split('/').collect::<Vec<_>>();
            ThinPoolBlockUsage {
                used_meta: meta_vals[0].parse::<u64>().unwrap(),
                total_meta: meta_vals[1].parse::<u64>().unwrap(),
                used_data: DataBlocks(data_vals[0].parse::<u64>().unwrap()),
                total_data: DataBlocks(data_vals[1].parse::<u64>().unwrap()),
            }
        };

        match status_vals[7] {
            "-" => {},
            "needs_check" => return Ok(ThinPoolStatus::Good(
                (ThinPoolWorkingStatus::NeedsCheck, usage))),
            _ => return Err(FroyoError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Kernel returned unexpected value in thin pool status")))
        }

        match status_vals[4] {
            "rw" => Ok(ThinPoolStatus::Good(
                (ThinPoolWorkingStatus::Good, usage))),
            "ro" => Ok(ThinPoolStatus::Good(
                (ThinPoolWorkingStatus::ReadOnly, usage))),
            "out_of_data_space" => Ok(ThinPoolStatus::Good(
                (ThinPoolWorkingStatus::OutOfSpace, usage))),
            _ => Err(FroyoError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Kernel returned unexpected value in thin pool status")))
        }
    }

    // return size of a data block in bytes
    pub fn data_block_size(&self) -> u64 {
        *self.data_block_size * SECTOR_SIZE
    }

    pub fn sectors_to_blocks(&self, sectors: Sectors) -> DataBlocks {
        DataBlocks(*sectors / *self.data_block_size)
    }

    pub fn blocks_to_sectors(&self, blocks: DataBlocks) -> Sectors {
        Sectors(*blocks * *self.data_block_size)
    }

    pub fn extend_data_dev(&mut self, segs: Vec<RaidSegment>)
                           -> FroyoResult<()> {
        try!(self.data_dev.borrow_mut().extend(segs));
        try!(self.dm_reload());
        Ok(())
    }

    pub fn extend_meta_dev(&mut self, segs: Vec<RaidSegment>)
                           -> FroyoResult<()> {
        try!(self.meta_dev.borrow_mut().extend(segs));
        try!(self.dm_reload());
        Ok(())
    }

    fn dm_reload(&self) -> FroyoResult<()> {
        let dm = try!(DM::new());
        let table = [(0u64, *self.data_dev.borrow().length(), "thin-pool", &*self.params)];

        try!(self.dev.reload(&dm, &table));

        Ok(())
    }

    pub fn used_sectors(&self) -> Sectors {
        self.meta_dev.borrow().length() + self.data_dev.borrow().length()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinDevSave {
    pub name: String,
    pub thin_number: u32,
    pub size: Sectors,
}

#[derive(Debug, Clone)]
pub struct ThinDev {
    dev: DmDevice,
    name: String,
    pub thin_number: u32,
    pub size: Sectors,
    dm_name: String,
    params: String,
}

#[derive(Debug, Clone, Copy)]
pub enum ThinStatus {
    Good(Sectors),
    Fail,
}

impl ThinDev {
    pub fn new(
        dm: &DM,
        froyo_id: &str,
        name: &str,
        thin_number: u32,
        size: Sectors,
        pool_dev: &ThinPoolDev)
        -> FroyoResult<ThinDev> {

        try!(pool_dev.dev.message(dm, &format!("create_thin {}", thin_number)));

        let mut td = try!(ThinDev::setup(
            dm,
            froyo_id,
            name,
            thin_number,
            size,
            pool_dev));

        try!(td.create_fs(name));

        Ok(td)
    }

    pub fn setup(
        dm: &DM,
        froyo_id: &str,
        name: &str,
        thin_number: u32,
        size: Sectors,
        pool_dev: &ThinPoolDev)
        -> FroyoResult<ThinDev> {

        let params = format!("{} {}", pool_dev.dev.dstr(), thin_number);
        let table = [(0u64, *size, "thin", &*params)];

        let dm_name = format!("froyo-thin-{}-{}", froyo_id, thin_number);
        let thin_dev = try!(DmDevice::new(dm, &dm_name, &table));

        try!(ThinDev::create_devnode(name, thin_dev.dev));

        let thin = ThinDev {
            dev: thin_dev,
            name: name.to_owned(),
            thin_number: thin_number,
            size: size,
            dm_name: dm_name,
            params: params.clone(),
        };

        if let ThinStatus::Fail = try!(thin.status()) {
            return Err(FroyoError::Froyo(InternalError(
                "Froyodev thin device is failed".into())))
        }

        Ok(thin)
    }

    pub fn teardown(&mut self, dm: &DM) -> FroyoResult<()> {
        // Do this first so if devnode is in use this fails before we
        // remove the devnode
        try!(self.dev.teardown(dm));
        try!(self.remove_devnode());

        Ok(())
    }

    pub fn extend(&mut self, sectors: Sectors) -> FroyoResult<()> {

        self.size = self.size + sectors;

        let dm = try!(DM::new());
        let id = &DevId::Name(&self.dm_name);

        let table = [(0u64, *self.size, "thin", &*self.params)];

        try!(dm.table_load(id, &table));
        try!(dm.device_suspend(id, DM_SUSPEND));
        try!(dm.device_suspend(id, DmFlags::empty()));

        // TODO: we need to know where it's mounted in order to call
        // this
        // let output = try!(Command::new("xfs_growfs")
        //                   .arg(&mount_point)
        //                   .output());

        Ok(())
    }

    pub fn to_save(&self) -> ThinDevSave {
        ThinDevSave {
            name: self.name.clone(),
            thin_number: self.thin_number,
            size: self.size,
        }
    }

    pub fn status(&self) -> FroyoResult<ThinStatus> {
        let dm = try!(DM::new());

        let (_, mut status) = try!(
            dm.table_status(&DevId::Name(&self.dm_name), DmFlags::empty()));

        if status.len() != 1 {
            return Err(FroyoError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected 1 line from thin status")))
        }

        // We should either get 1 line or the kernel is broken
        let status_line = status.pop().unwrap().3;
        if status_line.starts_with("Fail") {
            return Ok(ThinStatus::Fail)
        }
        let status_vals = status_line.split(' ').collect::<Vec<_>>();

        Ok(ThinStatus::Good(Sectors(
            status_vals[0].parse::<u64>().unwrap())))
    }

    fn create_devnode(name: &str, dev: Device) -> FroyoResult<()> {
        let mut pathbuf = PathBuf::from("/dev/froyo");

        if let Err(e) = fs::create_dir(&pathbuf) {
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(FroyoError::Io(e))
            }
        }

        pathbuf.push(name);

        let old_umask = umask(Mode::empty());
        let res = mknod(&pathbuf,
                    S_IFBLK,
                    S_IRUSR|S_IWUSR|S_IRGRP|S_IWGRP,
                    dev.into());
        umask(old_umask);
        if let Err(e) = res {
            if e.errno() != EEXIST {
                return Err(FroyoError::Nix(e))
            }
        }

        Ok(())
    }

    fn remove_devnode(&mut self) -> FroyoResult<()> {
        let mut pathbuf = PathBuf::from("/dev/froyo");
        pathbuf.push(&self.name);
        try!(fs::remove_file(&pathbuf));

        Ok(())
    }

    fn create_fs(&mut self, name: &str) -> FroyoResult<()> {
        let dev_name = format!("/dev/froyo/{}", name);
        let output = try!(Command::new("mkfs.xfs")
                          .arg("-f")
                          .arg(&dev_name)
                          .output());

        if output.status.success(){
            dbgp!("Created xfs filesystem on {}", dev_name)
        } else {
            return Err(FroyoError::Froyo(InternalError(
                format!("XFS mkfs error: {}",
                        String::from_utf8_lossy(&output.stderr)).into())))
        }
        Ok(())
    }
}
