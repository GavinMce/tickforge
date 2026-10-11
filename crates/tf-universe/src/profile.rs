//! The time-of-day profile of a symbol (E19-S53): average volume in each quarter hour of the premarket and the regular session, and the
//! normal absolute 60-minute move from the start of each quarter hour of the regular session.
//!
//! Kept small (a quarter hour of volume is a `u32` of shares, saturating; a move a `u32`, `u32::MAX` meaning unknown) because every
//! symbol's reference carries one and the host copies them. A strategy reads the volume of any run of minutes by adding the quarter hours.

use crate::feature::{CLOSE_MIN, OPEN_MIN, PREMARKET_START_MIN, QMOVE_COUNT, QVOL_COUNT};

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    vol: [u32; QVOL_COUNT],
    mv: [u32; QMOVE_COUNT],
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            vol: [NONE; QVOL_COUNT],
            mv: [NONE; QMOVE_COUNT],
        }
    }
}

fn pack(v: Option<i64>) -> u32 {
    match v {
        Some(x) if x >= 0 => u32::try_from(x).unwrap_or(NONE - 1).min(NONE - 1),
        _ => NONE,
    }
}

impl Profile {
    /// Average volume of quarter hour `k` (0 is 04:00 to 04:15, 22 is the open's, 47 the last), unknown where the history is short.
    pub fn vol_quarter(&self, k: usize) -> Option<i64> {
        self.vol
            .get(k)
            .filter(|v| **v != NONE)
            .map(|v| i64::from(*v))
    }

    pub fn set_vol_quarter(&mut self, k: usize, v: Option<i64>) {
        if let Some(slot) = self.vol.get_mut(k) {
            *slot = pack(v);
        }
    }

    /// The normal absolute move over the 60 minutes from regular-session quarter hour `j` (0 is 09:30), in hundred-thousandths of the
    /// price at its start.
    pub fn move_quarter(&self, j: usize) -> Option<i64> {
        self.mv
            .get(j)
            .filter(|v| **v != NONE)
            .map(|v| i64::from(*v))
    }

    pub fn set_move_quarter(&mut self, j: usize, v: Option<i64>) {
        if let Some(slot) = self.mv.get_mut(j) {
            *slot = pack(v);
        }
    }

    /// The average volume in the minutes `from` to `to` (minutes after midnight New York time, `from` before `to`, both from 04:00 to 16:00),
    /// the quarter hours added, each in proportion to the minutes of it the run covers (volume is taken as even within a quarter hour, so a
    /// run that is not a whole number of quarter hours is an estimate and one that is whole is exact). Unknown if any quarter hour it touches
    /// is, or if the run is empty or outside the day.
    pub fn volume_between(&self, from: u32, to: u32) -> Option<i64> {
        if from >= to || from < PREMARKET_START_MIN || to > CLOSE_MIN {
            return None;
        }
        let first = (from - PREMARKET_START_MIN) / 15;
        let last = (to - 1 - PREMARKET_START_MIN) / 15;
        let mut weighted: i128 = 0;
        for k in first..=last {
            let start = PREMARKET_START_MIN + 15 * k;
            let covered = to.min(start + 15) - from.max(start);
            weighted += i128::from(self.vol_quarter(k as usize)?) * i128::from(covered);
        }
        i64::try_from(weighted / 15).ok()
    }

    /// The normal absolute 60-minute move from the quarter hour that starts at `minute` (minutes after midnight, a multiple of 15 from 09:30
    /// to 15:00); unknown for any other minute.
    pub fn move_from(&self, minute: u32) -> Option<i64> {
        if minute < OPEN_MIN || (minute - OPEN_MIN) % 15 != 0 {
            return None;
        }
        self.move_quarter(((minute - OPEN_MIN) / 15) as usize)
    }
}
