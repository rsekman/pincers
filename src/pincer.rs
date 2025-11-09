use std::collections::{BTreeMap, HashMap};

use spdlog::prelude::*;

use crate::error::Error;
use crate::register::{MimeType, NumericT, Register, RegisterAddress, RegisterSummary};
use crate::seat::SeatIdentifier;

/// Struct for the state of the clipboard manager
#[derive(Debug)]
pub struct Pincer {
    // The Pincer's currently active register
    active: RegisterAddress,
    last_used: RegisterAddress,
    // To avoid moving data as yanks to "0 shift higher-numbered registers up, we will treat the
    // `numeric` array as a circular buffer. This is the offset in that circular buffer.
    pointer: NumericT,
    numeric: [Register; 10],
    named: [Register; 26],
}

pub type SeatPincerMap = HashMap<SeatIdentifier, Pincer>;

impl Pincer {
    /// Create a new Pincer
    pub fn new() -> Self {
        Pincer {
            active: RegisterAddress::Unnamed,
            last_used: RegisterAddress::Numeric(NumericT::new(0).unwrap()),
            pointer: NumericT::new(0).unwrap(),
            numeric: Default::default(),
            named: Default::default(),
        }
    }

    /// Set the Pincer's register pointer
    pub fn set_active(&mut self, addr: RegisterAddress) {
        self.active = addr;
    }
    /// Get the Pincer's register pointer
    pub fn get_active(&self) -> RegisterAddress {
        self.active
    }
    pub fn reset_active(&mut self) {
        self.set_active(RegisterAddress::default());
    }

    /// Get a register from a register address
    ///
    /// # Arguments
    ///
    /// * `addr` -  address to the register.
    pub fn register(&self, addr: RegisterAddress) -> &Register {
        use RegisterAddress::*;
        match addr {
            Numeric(n) => self
                .numeric
                .get(shift_backward(n, self.pointer).get() as usize),
            Named(n) => self.named.get(n.get() as usize),
            Unnamed => Some(self.register(self.last_used)),
        }
        .unwrap()
    }

    pub fn get_active_register(&self) -> &Register {
        self.register(self.get_active())
    }

    fn resolve(&self, addr: RegisterAddress) -> RegisterAddress {
        match addr {
            RegisterAddress::Unnamed => self.last_used,
            _ => addr,
        }
    }

    /// Get the data contained in a register
    ///
    /// # Arguments
    ///
    /// * `addr` -  address to the register to paste from. `Unnamed` means the last yank
    /// * `mime` -  MIME type to get
    ///
    /// # Returns
    ///
    /// If the MIME type exists in the register, `Some((addr, buffer))` where `addr` is the register
    /// that was actually used, `buffer` contains the data.  Otherwise `None`.
    pub fn paste_from<'a>(
        &'a self,
        addr: RegisterAddress,
        mime: &'a MimeType,
    ) -> Option<(RegisterAddress, &'a MimeType, &'a Vec<u8>)> {
        let raw_addr = self.resolve(addr);
        let (mime, res) = self.register(addr).get(mime)?;
        Some((raw_addr, mime, res))
    }

    /// Get data from the currently active register
    ///
    /// # Arguments
    ///
    /// * `mime` -  MIME type to get
    ///
    /// # Returns
    ///
    /// If the MIME type exists in the register, `Ok(buffer)` where `buffer` contains the data.
    /// Otherwise `Err`.
    pub fn paste<'a>(
        &'a self,
        mime: &'a MimeType,
    ) -> Option<(RegisterAddress, &'a MimeType, &'a Vec<u8>)> {
        self.paste_from(self.active, mime)
    }

    fn advance_pointer(&mut self) {
        self.pointer = shift_forward(self.pointer, NumericT::new(1).unwrap())
    }

    /// Yank data of multiple MIME types into a register
    ///
    /// # Arguments
    ///
    /// * `addr` -  address to the register to paste from. Unnamed means the default, `"0`
    /// * `pastes` - an iterator over `(MIME, buffer)` tuples
    ///
    /// # Returns
    ///
    /// `Ok(n)` where `n` is the total number of bytes yanked if successful,
    /// otherwise `Err`.
    pub fn yank_into<T>(
        &mut self,
        addr: RegisterAddress,
        pastes: T,
    ) -> Result<(RegisterAddress, usize), Error>
    where
        T: Iterator<Item = (MimeType, Vec<u8>)>,
    {
        use RegisterAddress::*;
        let z = NumericT::new(0).unwrap();
        let reg = match addr {
            Numeric(n) => self
                .numeric
                .get_mut(shift_backward(n, self.pointer).get() as usize),
            Named(n) => self.named.get_mut(n.get() as usize),
            Unnamed => {
                self.advance_pointer();
                self.numeric
                    .get_mut(shift_backward(z, self.pointer).get() as usize)
            }
        }
        .unwrap();
        reg.clear();
        let mut bytes = 0;
        for (mime, data) in pastes {
            bytes += data.len();
            reg.insert(mime, data);
        }

        self.last_used = match addr {
            Unnamed => RegisterAddress::Numeric(z),
            _ => addr,
        };
        debug!("Yanked {bytes} bytes into {}", self.last_used);
        Ok((self.last_used, bytes))
    }

    /// Yank data of a single MIME type into a register
    ///
    /// # Arguments
    ///
    /// * `addr` -  address to the register to paste from. `None` means the default, `"0`
    /// * `(mime, data)` - the MIME type of the data and a buffer where it is stored
    ///
    /// # Returns
    ///
    /// `Ok((addr, n))` where `addr` is the register that was used and `n` is the total number of
    /// bytes yanked if successful, otherwise `Err`.
    pub fn yank_one_into(
        &mut self,
        addr: RegisterAddress,
        (mime, data): (MimeType, Vec<u8>),
    ) -> Result<(RegisterAddress, usize), Error> {
        self.yank_into(addr, std::iter::once((mime, data)))
    }

    /// Yank data of multiple MIME types into the currently active register
    ///
    /// # Arguments
    ///
    /// * `pastes` - an iterator over `(MIME, buffer)` tuples
    ///
    /// # Returns
    ///
    /// `Ok((addr, n))` where `addr` is the register that was used and `n` is the total number of
    /// bytes yanked if successful, otherwise `Err`.
    pub fn yank<T>(&mut self, pastes: T) -> Result<(RegisterAddress, usize), Error>
    where
        T: Iterator<Item = (MimeType, Vec<u8>)>,
    {
        self.yank_into(self.active, pastes)
    }

    /// Yank data of a single MIME type into the currently active register
    ///
    /// # Arguments
    ///
    /// * `(mime, data)` - the MIME type of the data and a buffer where it is stored
    ///
    /// # Returns
    ///
    /// `Ok(n)` where `n` is the total number of bytes yanked if successful,
    /// otherwise `Err`.
    pub fn yank_one(
        &mut self,
        (mime, data): (MimeType, Vec<u8>),
    ) -> Result<(RegisterAddress, usize), Error> {
        self.yank_one_into(self.active, (mime, data))
    }

    /// Summarize the contents of all registers
    ///
    /// # Returns
    ///
    /// `Ok(m)` where `m` is a map from register addresses to summarise of their contents if
    /// successful, otherwise `Err`.
    pub fn list(&self) -> Result<BTreeMap<RegisterAddress, RegisterSummary>, Error> {
        let mut out = BTreeMap::new();
        //out.insert(RegisterAddress::Unnamed, )
        out.extend(RegisterAddress::iter().filter_map(|addr| {
            let r = self.register(addr);
            if !r.is_empty() {
                Some((addr, r.summarize()))
            } else {
                None
            }
        }));
        Ok(out)
    }
}

impl Default for Pincer {
    fn default() -> Self {
        Self::new()
    }
}

fn shift_forward(x: NumericT, y: NumericT) -> NumericT {
    let z = (x.get() + y.get()).rem_euclid(NumericT::MAX_VALUE + 1);
    NumericT::new(z).unwrap()
}

fn shift_backward(x: NumericT, y: NumericT) -> NumericT {
    let z = (x.get() - y.get()).rem_euclid(NumericT::MAX_VALUE + 1);
    NumericT::new(z).unwrap()
}
