// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::num::Zero;
use std::io;
use std::fmt;
use std::error::Error;

use serde;
use serde_json;
use nix;
use term;

pub type FroyoResult<T> = Result<T, FroyoError>;

//
// Use distinct 'newtype' types for sectors and sector offsets for type safety.
// When needed, these can still be derefed to u64.
// Derive a bunch of stuff so we can do ops on them.
//
custom_derive! {
    #[derive(NewtypeFrom, NewtypeAdd, NewtypeSub, NewtypeDeref,
             NewtypeBitAnd, NewtypeNot, NewtypeDiv, NewtypeRem,
             NewtypeMul,
             Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord)]
    pub struct Sectors(u64);
}

impl Sectors {
    pub const fn new(val: u64) -> Sectors {
        Sectors(val)
    }
}

impl Zero for Sectors {
    fn zero() -> Self {
        Sectors(0)
    }
}

impl serde::Serialize for Sectors {
    fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
        where S: serde::Serializer,
    {
        serializer.serialize_u64(**self)
    }
}

impl serde::Deserialize for Sectors {
    fn deserialize<D>(deserializer: &mut D) -> Result<Sectors, D::Error>
        where D: serde::de::Deserializer
    {
        let val = try!(serde::Deserialize::deserialize(deserializer));
        Ok(Sectors(val))
    }
}

custom_derive! {
    #[derive(NewtypeFrom, NewtypeAdd, NewtypeSub, NewtypeDeref,
             NewtypeBitAnd, NewtypeNot, NewtypeDiv, NewtypeRem,
             NewtypeMul,
             Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord)]
    pub struct SectorOffset(u64);
}

impl SectorOffset {
    pub const fn new(val: u64) -> SectorOffset {
        SectorOffset(val)
    }
}

impl Zero for SectorOffset {
    fn zero() -> Self {
        SectorOffset(0)
    }
}

impl serde::Serialize for SectorOffset {
    fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
        where S: serde::Serializer,
    {
        serializer.serialize_u64(**self)
    }
}

impl serde::Deserialize for SectorOffset {
    fn deserialize<D>(deserializer: &mut D) -> Result<SectorOffset, D::Error>
        where D: serde::de::Deserializer
    {
        let val = try!(serde::Deserialize::deserialize(deserializer));
        Ok(SectorOffset(val))
    }
}

// A type for Data Blocks as used by the thin pool.
custom_derive! {
    #[derive(NewtypeFrom, NewtypeAdd, NewtypeSub, NewtypeDeref,
             NewtypeBitAnd, NewtypeNot, NewtypeDiv, NewtypeRem,
             NewtypeMul,
             Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord)]
    pub struct DataBlocks(u64);
}

impl DataBlocks {
    pub const fn new(val: u64) -> DataBlocks {
        DataBlocks(val)
    }
}

impl Zero for DataBlocks {
    fn zero() -> Self {
        DataBlocks(0)
    }
}

impl serde::Serialize for DataBlocks {
    fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
        where S: serde::Serializer,
    {
        serializer.serialize_u64(**self)
    }
}

impl serde::Deserialize for DataBlocks {
    fn deserialize<D>(deserializer: &mut D) -> Result<DataBlocks, D::Error>
        where D: serde::de::Deserializer
    {
        let val = try!(serde::Deserialize::deserialize(deserializer));
        Ok(DataBlocks(val))
    }
}

//
// An error type for errors generated within Froyo
//
#[derive(Debug, Clone)]
pub struct InternalError(pub String);

impl fmt::Display for InternalError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "{}", self.0)
    }
}

impl Error for InternalError {
    fn description(&self) -> &str {
        &self.0
    }
}

// Define a common error enum.
// See http://blog.burntsushi.net/rust-error-handling/
#[derive(Debug)]
pub enum FroyoError {
    Froyo(InternalError),
    Io(io::Error),
    Serde(serde_json::error::Error),
    Nix(nix::Error),
    Dbus(()),
    Term(term::Error),
}

impl fmt::Display for FroyoError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            FroyoError::Froyo(ref err) => write!(f, "Froyo error: {}", err.0),
            FroyoError::Io(ref err) => write!(f, "IO error: {}", err),
            FroyoError::Serde(ref err) => write!(f, "Serde error: {}", err),
            FroyoError::Nix(ref err) => write!(f, "Nix error: {}", err.errno().desc()),
            FroyoError::Dbus(()) => write!(f, "Dbus error"),
            FroyoError::Term(ref err) => write!(f, "Term error: {}", err),
        }
    }
}

impl Error for FroyoError {
    fn description(&self) -> &str {
        match *self {
            FroyoError::Froyo(ref err) => &err.0,
            FroyoError::Io(ref err) => err.description(),
            FroyoError::Serde(ref err) => Error::description(err),
            FroyoError::Nix(ref err) => err.errno().desc(),
            FroyoError::Dbus(()) => "Dbus error",
            FroyoError::Term(ref err) => Error::description(err),
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            FroyoError::Froyo(ref err) => Some(err),
            FroyoError::Io(ref err) => Some(err),
            FroyoError::Serde(ref err) => Some(err),
            FroyoError::Nix(ref err) => Some(err),
            FroyoError::Dbus(()) => None,
            FroyoError::Term(ref err) => Some(err),
        }
    }
}

impl From<InternalError> for FroyoError {
    fn from(err: InternalError) -> FroyoError {
        FroyoError::Froyo(err)
    }
}

impl From<io::Error> for FroyoError {
    fn from(err: io::Error) -> FroyoError {
        FroyoError::Io(err)
    }
}

impl From<serde_json::error::Error> for FroyoError {
    fn from(err: serde_json::error::Error) -> FroyoError {
        FroyoError::Serde(err)
    }
}

impl From<nix::Error> for FroyoError {
    fn from(err: nix::Error) -> FroyoError {
        FroyoError::Nix(err)
    }
}

impl From<()> for FroyoError {
    fn from(err: ()) -> FroyoError {
        FroyoError::Dbus(err)
    }
}

impl From<term::Error> for FroyoError {
    fn from(err: term::Error) -> FroyoError {
        FroyoError::Term(err)
    }
}
