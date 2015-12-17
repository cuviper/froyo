// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::cell::RefCell;
use std::borrow::Borrow;
use std::path::Path;
use std::cmp::Ordering;
use std::io;
use std::io::ErrorKind;
use std::error::Error;

use uuid::Uuid;
use devicemapper::DM;
use serde_json;
use time;

use blockdev::{BlockDev, BlockDevSave};
use blockdev::{LinearDev, LinearSegment};
use raid::{RaidDev, RaidDevSave, RaidSegment, RaidLinearDev, RaidStatus};
use thin::{ThinPoolDev, ThinPoolDevSave};
use thin::{ThinDev, ThinDevSave};
use types::{Sectors, SectorOffset, FroyoError};
use util::{align_to, clear_dev};
use consts::*;


#[derive(Debug, Clone, Serialize, Deserialize)]
struct FroyoSave {
    name: String,
    id: String,
    block_devs: BTreeMap<String, BlockDevSave>,
    raid_devs: BTreeMap<String, RaidDevSave>,
    thin_pool_dev: ThinPoolDevSave,
    thin_devs: Vec<ThinDevSave>,
}

#[derive(Debug, Clone)]
pub struct Froyo {
    id: String,
    pub name: String,
    block_devs: BTreeMap<String, Rc<RefCell<BlockDev>>>,
    raid_devs: BTreeMap<String, Rc<RefCell<RaidDev>>>,
    thin_pool_dev: ThinPoolDev,
    thin_devs: Vec<ThinDev>,
    throttled: bool,
}

pub enum FroyoStatus {
    Good,
    Degraded(usize),
    Failed,
}

pub enum FroyoPerfStatus {
    Good,
    Throttled,
}

impl Froyo {
    pub fn create<T>(name: &str, id: &str, paths: &[T], force: bool) -> Result<Froyo, FroyoError>
        where T: Borrow<Path>
    {
        let mut block_devs = BTreeMap::new();
        for path in paths {
            let bd = Rc::new(RefCell::new(
                try!(BlockDev::initialize(&id, path.borrow(), force))));
            block_devs.insert(RefCell::borrow(&bd).id.clone(), bd.clone());
        }

        if paths.len() < 2 {
            return Err(FroyoError::Io(io::Error::new(
                ErrorKind::InvalidInput, "At least 2 block devices must be given")))
        }

        if paths.len() > 8 {
            return Err(FroyoError::Io(io::Error::new(
                ErrorKind::InvalidInput,
                format!("Max supported devices is 8, {} given", paths.len()))))
        }

        let dm = try!(DM::new());

        let mut raid_devs = BTreeMap::new();
        loop {
            if let Some(rd) = try!(
                Froyo::create_redundant_zone(&dm, name, &block_devs, force)) {
                raid_devs.insert(rd.id.clone(), Rc::new(RefCell::new(rd)));
            } else {
                break
            }
        }

        let thin_pool_dev = try!(ThinPoolDev::new(&dm, name, &raid_devs));
        let mut thin_devs = Vec::new();

        // Create an initial 1TB thin dev
        thin_devs.push(try!(ThinDev::create(
            &dm,
            name,
            0,
            "xfs",
            Sectors::new(1024 * 1024 * 1024 * 1024 / SECTOR_SIZE),
            &thin_pool_dev)));

        Ok(Froyo {
            name: name.to_owned(),
            id: id.to_owned(),
            block_devs: block_devs,
            raid_devs: raid_devs,
            thin_pool_dev: thin_pool_dev,
            thin_devs: thin_devs,
            throttled: false,
        })
    }

    pub fn to_save(&self) -> FroyoSave {
        FroyoSave {
            name: self.name.to_owned(),
            id: self.id.to_owned(),
            block_devs: self.block_devs.iter()
                .map(|(id, bd)| (id.clone(), RefCell::borrow(bd).to_save()))
                .collect(),
            raid_devs: self.raid_devs.iter()
                .map(|(id, rd)| (id.clone(), RefCell::borrow(rd).to_save()))
                .collect(),
            thin_pool_dev: self.thin_pool_dev.to_save(),
            thin_devs: self.thin_devs.iter()
                .map(|x| x.to_save())
                .collect(),
        }
    }

    pub fn find_all() -> Result<Vec<Froyo>, FroyoError> {
        // We could have BlockDevs for multiple Froyodevs.
        // Group them by Froyo uuid.
        let mut froyo_devs = BTreeMap::new();
        for bd in try!(BlockDev::find_all()) {
            froyo_devs.entry(bd.froyodev_id.clone())
                .or_insert(Vec::new())
                .push(bd);
        }

        let mut froyos = Vec::new();
        for (froyo_id, bds) in froyo_devs {
            let buf = {
                // get newest metadata across all blockdevs and in either MDA
                let newest_bd = bds.iter()
                    .map(|bd| {
                        let mda = match bd.mdaa.last_updated.cmp(&bd.mdab.last_updated) {
                            Ordering::Less => &bd.mdab,
                            Ordering::Greater => &bd.mdaa,
                            Ordering::Equal => &bd.mdab,
                        };
                        (mda.last_updated, bd)
                    })
                    .max_by_key(|&(tm, _)| tm)
                    .unwrap().1;
                try!(newest_bd.read_mdax())
            };
            let s = String::from_utf8_lossy(&buf).into_owned();

            let froyo_save = try!(serde_json::from_str::<FroyoSave>(&s));

            froyos.push(try!(Froyo::from_save(froyo_save, froyo_id, bds)));
        }

        Ok(froyos)
    }

    pub fn find(name: &str) -> Result<Option<Froyo>, FroyoError> {
        let froyos = try!(Froyo::find_all());
        for f in froyos {
            if name == f.name {
                return Ok(Some(f))
            }
        }

        Ok(None)
    }

    fn from_save(froyo_save: FroyoSave, froyo_id: String, blockdevs: Vec<BlockDev>)
                 -> Result<Froyo, FroyoError> {
        let mut bd_map = blockdevs.into_iter()
            .map(|x| (x.id.clone(), x))
            .collect::<BTreeMap<_, _>>();

        let mut block_devs = BTreeMap::new();
        for (id, sbd) in &froyo_save.block_devs {
            match bd_map.remove(id) {
                Some(x) => { block_devs.insert(id.clone(), Rc::new(RefCell::new(x))); },
                None => ::dbgp!("missing a blockdev: id {} path {}", id,
                              sbd.path.display()),
            }
        }

        for (_, bd) in bd_map {
            dbgp!("{} header indicates it's part of {} but not found in metadata",
                  bd.path.display(), froyo_save.name);
        }

        match froyo_save.block_devs.len() - block_devs.len() {
            0 => dbgp!("All {} block devices found for {}",
                       block_devs.len(), froyo_save.name),
            num @ 1...FROYO_REDUNDANCY => dbgp!("Missing {} of {} drives from {}, can continue",
                                                num, froyo_save.block_devs.len(), froyo_save.name),
            num @ _ => return Err(FroyoError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} of {} devices missing from {}",
                        num, froyo_save.block_devs.len(), froyo_save.name)))),
        }

        let dm = try!(DM::new());

        let mut raid_devs = BTreeMap::new();
        for (id, srd) in froyo_save.raid_devs.iter() {
            let mut linear_devs = Vec::new();
            for (m_num, sld) in srd.members.iter().enumerate() {
                match block_devs.get(&sld.parent) {
                    Some(bd) => {
                        let ld = Rc::new(RefCell::new(try!(LinearDev::create(
                            &dm, &format!("{}-{}-{}", froyo_save.name, id, m_num),
                            bd, &sld.meta_segments, &sld.data_segments))));

                        bd.borrow_mut().linear_devs.push(ld.clone());
                        linear_devs.push(ld);
                    },
                    None => dbgp!("could not find parent {} for a linear device", sld.parent),
                }
            }

            // TODO: handle when devs is less than what's in srd
            let rd = Rc::new(RefCell::new(try!(RaidDev::create(
                &dm,
                &froyo_save.name,
                id.clone(),
                linear_devs,
                srd.stripe_sectors,
                srd.region_sectors))));

            let id = RefCell::borrow(&rd).id.clone();
            raid_devs.insert(id, rd);
        }

        let thin_pool_dev = {
            let tpd = &froyo_save.thin_pool_dev;

            let meta_name = format!("thin-meta-{}", froyo_save.name);
            let mut raid_segments = Vec::new();
            for seg in &tpd.meta_dev.segments {
                let parent = try!(raid_devs.get(&seg.parent).ok_or(
                    io::Error::new(io::ErrorKind::InvalidInput,
                                   "Could not find meta's parent")));
                raid_segments.push(
                    RaidSegment::new(seg.start, seg.length, parent));
            }

            let meta_raid_dev = try!(RaidLinearDev::create(
                &dm,
                &meta_name,
                &tpd.meta_dev.id,
                raid_segments));

            let data_name = format!("thin-data-{}", froyo_save.name);
            let mut raid_segments = Vec::new();
            for seg in &tpd.data_dev.segments {
                let parent = try!(raid_devs.get(&seg.parent).ok_or(
                    io::Error::new(io::ErrorKind::InvalidInput,
                                   "Could not find data's parent")));
                raid_segments.push(
                    RaidSegment::new(seg.start, seg.length, parent));
            }

            let data_raid_dev = try!(RaidLinearDev::create(
                &dm,
                &data_name,
                &tpd.data_dev.id,
                raid_segments));

            try!(ThinPoolDev::create(
                &dm,
                &froyo_save.name,
                tpd.data_block_size,
                tpd.low_water_blocks,
                meta_raid_dev,
                data_raid_dev))
        };

        let mut thin_devs = Vec::new();
        for std in &froyo_save.thin_devs {
            thin_devs.push(try!(ThinDev::create(
                &dm,
                &froyo_save.name,
                std.thin_number,
                &std.fs,
                std.size,
                &thin_pool_dev)));
        }

        Ok(Froyo {
            name: froyo_save.name.to_owned(),
            id: froyo_id.to_owned(),
            block_devs: block_devs,
            raid_devs: raid_devs,
            thin_pool_dev: thin_pool_dev,
            thin_devs: thin_devs,
            throttled: false,
        })
    }

    // Try to make an as-large-as-possible redundant device from the
    // given block devices.
    fn create_redundant_zone(
        dm: &DM,
        name: &str,
        block_devs: &BTreeMap<String, Rc<RefCell<BlockDev>>>,
        force: bool)
        -> Result<Option<RaidDev>, FroyoError> {

        // TODO: Make sure name has only chars we can use in a DM name

        // get common data area size, allowing for Froyo data at start and end
        let mut bd_areas: Vec<_> = block_devs.iter()
            .filter_map(|(_, bd)| {
                match RefCell::borrow(bd).largest_free_area() {
                    Some(x) => Some((bd.clone(), x)),
                    None => None,
                }
            })
            .filter(|&(_, (_, len))| len >= MIN_DATA_ZONE_SECTORS)
            .collect();

        // Not enough devs with room for a raid device
        if bd_areas.len() < 2 {
            return Ok(None)
        }

        let common_free_sectors = bd_areas.iter()
            .map(|&(_, (_, len))| len)
            .min()
            .unwrap();

        let (region_count, region_sectors) = {
            let mut region_sectors = DEFAULT_REGION_SECTORS;
            while *common_free_sectors / *region_sectors > MAX_REGIONS {
                region_sectors = Sectors::new(*region_sectors * 2);
            }

            let partial_region = match common_free_sectors % region_sectors == Sectors::new(0) {
                true => Sectors::new(0),
                false => Sectors::new(1),
            };

            (common_free_sectors / region_sectors + partial_region, region_sectors)
        };

        // each region needs 1 bit in the write intent bitmap
        let mdata_sectors = Sectors::new(align_to(8192 + (*region_count / 8) , SECTOR_SIZE)
                                    .next_power_of_two()
                                    / SECTOR_SIZE);
        // data size must be multiple of stripe size
        let data_sectors = (common_free_sectors - mdata_sectors) & Sectors::new(!(*STRIPE_SECTORS-1));

        let raid_uuid = Uuid::new_v4().to_simple_string();

        let mut linear_devs = Vec::new();
        for (num, &mut(ref mut bd, (sector_start, _))) in bd_areas.iter_mut().enumerate() {
            let mdata_sector_start = sector_start;
            let data_sector_start = SectorOffset::new(*mdata_sector_start + *mdata_sectors);

            let linear = Rc::new(RefCell::new(try!(LinearDev::create(
                &dm,
                &format!("{}-{}-{}", name, raid_uuid, num),
                bd,
                &vec![LinearSegment {
                    start: mdata_sector_start,
                    length: mdata_sectors,
                }],
                &vec![LinearSegment {
                    start: data_sector_start,
                    length: data_sectors,
                    }]))));

            if force {
                try!(clear_dev(&RefCell::borrow(&linear).meta_dev));
            }

            bd.borrow_mut().linear_devs.push(linear.clone());
            linear_devs.push(linear);
        }

        let raid = try!(RaidDev::create(
            &dm,
            &name,
            raid_uuid,
            linear_devs,
            STRIPE_SECTORS,
            region_sectors));

        Ok(Some(raid))
    }

    pub fn save_state(&self) -> Result<(), FroyoError> {
        let froyo_info = self.to_save();
        let metadata = try!(serde_json::to_string(&froyo_info));
        let current_time = time::now().to_timespec();

        for (_, bd) in &self.block_devs {
            try!(bd.borrow_mut().save_state(&current_time, metadata.as_bytes()))
        }

        Ok(())
    }

    pub fn status(&self) -> io::Result<(FroyoStatus, FroyoPerfStatus)> {

        let mut status = FroyoStatus::Good;
        for (_, rd) in &self.raid_devs {
            let rd = RefCell::borrow(rd);
            match try!(rd.status()) {
                RaidStatus::Failed => {
                    status = FroyoStatus::Failed;
                    break
                },
                RaidStatus::Degraded(x) => status = FroyoStatus::Degraded(x),
                RaidStatus::Good => {},
            }
        }

        let perf_status = match self.throttled {
            true => FroyoPerfStatus::Throttled,
            false => FroyoPerfStatus::Good,
        };

        Ok((status, perf_status))
    }
}
