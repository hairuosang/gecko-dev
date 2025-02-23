// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Tracking of received packets and generating acks thereof.

use std::cmp::min;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::ops::{Index, IndexMut};
use std::time::{Duration, Instant};

use neqo_common::{qdebug, qinfo, qtrace, qwarn};
use neqo_crypto::constants::Epoch;

use crate::frame::{AckRange, Frame};
use crate::recovery::RecoveryToken;

// TODO(mt) look at enabling EnumMap for this: https://stackoverflow.com/a/44905797/1375574
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PNSpace {
    Initial = 0,
    Handshake = 1,
    ApplicationData = 2,
}

impl PNSpace {
    pub fn iter() -> impl Iterator<Item = &'static PNSpace> {
        const SPACES: &[PNSpace] = &[
            PNSpace::Initial,
            PNSpace::Handshake,
            PNSpace::ApplicationData,
        ];
        SPACES.iter()
    }
}

impl From<Epoch> for PNSpace {
    fn from(epoch: Epoch) -> Self {
        match epoch {
            0 => PNSpace::Initial,
            2 => PNSpace::Handshake,
            _ => PNSpace::ApplicationData,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PacketRange {
    largest: u64,
    smallest: u64,
    ack_needed: bool,
}

impl PacketRange {
    /// Make a single packet range.
    pub fn new(pn: u64) -> Self {
        Self {
            largest: pn,
            smallest: pn,
            ack_needed: true,
        }
    }

    /// Get the number of acknowleged packets in the range.
    pub fn len(&self) -> u64 {
        self.largest - self.smallest + 1
    }

    /// Returns whether this needs to be sent.
    pub fn ack_needed(&self) -> bool {
        self.ack_needed
    }

    /// Return whether the given number is in the range.
    pub fn contains(&self, pn: u64) -> bool {
        (pn >= self.smallest) && (pn <= self.largest)
    }

    /// Maybe add a packet number to the range.  Returns true if it was added.
    pub fn add(&mut self, pn: u64) -> bool {
        assert!(!self.contains(pn));
        // Only insert if this is adjacent the current range.
        if (self.largest + 1) == pn {
            qtrace!([self] "Adding largest {}", pn);
            self.largest += 1;
            self.ack_needed = true;
            true
        } else if self.smallest == (pn + 1) {
            qtrace!([self] "Adding smallest {}", pn);
            self.smallest -= 1;
            self.ack_needed = true;
            true
        } else {
            false
        }
    }

    /// Maybe merge a lower-numbered range into this.
    pub fn merge_smaller(&mut self, other: &Self) {
        qinfo!([self] "Merging {}", other);
        // This only works if they are immediately adjacent.
        assert_eq!(self.smallest - 1, other.largest);

        self.smallest = other.smallest;
        self.ack_needed = self.ack_needed || other.ack_needed;
    }

    /// When a packet containing the range `other` is acknowledged,
    /// clear the ack_needed attribute on this.
    /// Requires that other is equal to this, or a larger range.
    pub fn acknowledged(&mut self, other: &Self) {
        if (other.smallest <= self.smallest) && (other.largest >= self.largest) {
            qinfo!([self] "Acknowledged");
            self.ack_needed = false;
        }
    }
}

impl ::std::fmt::Display for PacketRange {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "{}->{}", self.largest, self.smallest)
    }
}

/// The ACK delay we use.
pub const ACK_DELAY: Duration = Duration::from_millis(20); // 20ms
const MAX_TRACKED_RANGES: usize = 100;
const MAX_ACKS_PER_FRAME: usize = 32;

/// A structure that tracks what was included in an ACK.
#[derive(Debug, Clone)]
pub struct AckToken {
    space: PNSpace,
    ranges: Vec<PacketRange>,
}

/// A structure that tracks what packets have been received,
/// and what needs acknowledgement for a packet number space.
#[derive(Debug)]
pub struct RecvdPackets {
    space: PNSpace,
    ranges: VecDeque<PacketRange>,
    /// The packet number of the lowest number packet that we are tracking.
    min_tracked: u64,
    /// The time we got the largest acknowledged.
    largest_pn_time: Option<Instant>,
    // The time that we should be sending an ACK.
    ack_time: Option<Instant>,
}

impl RecvdPackets {
    /// Make a new RecvdPackets for the indicated packet number space.
    pub fn new(space: PNSpace) -> Self {
        Self {
            space,
            ranges: VecDeque::new(),
            min_tracked: 0,
            largest_pn_time: None,
            ack_time: None,
        }
    }

    /// Get the time at which the next ACK should be sent.
    pub fn ack_time(&self) -> Option<Instant> {
        self.ack_time
    }

    /// Returns true if an ACK frame should be sent now.
    fn ack_now(&self, now: Instant) -> bool {
        match self.ack_time {
            Some(t) => t <= now,
            _ => false,
        }
    }

    // A simple addition of a packet number to the tracked set.
    // This doesn't do a binary search on the assumption that
    // new packets will generally be added to the start of the list.
    fn add(&mut self, pn: u64) -> usize {
        for i in 0..self.ranges.len() {
            if self.ranges[i].add(pn) {
                // Maybe merge two ranges.
                let nxt = i + 1;
                if (nxt < self.ranges.len()) && (pn - 1 == self.ranges[nxt].largest) {
                    let smaller = self.ranges.remove(nxt).unwrap();
                    self.ranges[i].merge_smaller(&smaller);
                }
                return i;
            }
            if self.ranges[i].largest < pn {
                self.ranges.insert(i, PacketRange::new(pn));
                return i;
            }
        }
        self.ranges.push_back(PacketRange::new(pn));
        self.ranges.len() - 1
    }

    /// Add the packet to the tracked set.
    pub fn set_received(&mut self, now: Instant, pn: u64, ack_eliciting: bool) {
        let i = self.add(pn);

        // The new addition was the largest, so update the time we use for calculating ACK delay.
        if i == 0 && pn == self.ranges[0].largest {
            self.largest_pn_time = Some(now);
        }

        // Limit the number of ranges that are tracked to MAX_TRACKED_RANGES.
        if self.ranges.len() > MAX_TRACKED_RANGES {
            let oldest = self.ranges.pop_back().unwrap();
            if oldest.ack_needed {
                qwarn!([self] "Dropping unacknowledged ACK range: {}", oldest);
            // TODO(mt) Record some statistics about this so we can tune MAX_TRACKED_RANGES.
            } else {
                qdebug!([self] "Drop ACK range: {}", oldest);
            }
            self.min_tracked = oldest.largest + 1;
        }

        if ack_eliciting {
            // On the first ack-eliciting packet since sending an ACK, set a delay.
            // On the second, remove that delay.
            if self.ack_time.is_none() && self.space == PNSpace::ApplicationData {
                self.ack_time = Some(now + ACK_DELAY);
            } else {
                self.ack_time = Some(now);
            }
        }
    }

    /// Check if the packet is a duplicate.
    pub fn is_duplicate(&self, pn: u64) -> bool {
        if pn < self.min_tracked {
            return true;
        }
        // TODO(mt) consider a binary search or early exit.
        for range in &self.ranges {
            if range.contains(pn) {
                return true;
            }
        }
        false
    }

    /// Mark the given range as having been acknowledged.
    pub fn acknowledged(&mut self, acked: &[PacketRange]) {
        let mut range_iter = self.ranges.iter_mut();
        let mut cur = range_iter.next().expect("should have at least one range");
        for ack in acked {
            while cur.smallest > ack.largest {
                cur = match range_iter.next() {
                    Some(c) => c,
                    _ => return,
                };
            }
            cur.acknowledged(&ack);
        }
    }
}

impl ::std::fmt::Display for RecvdPackets {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "Recvd{:?}", self.space)
    }
}

#[derive(Debug)]
pub struct AckTracker {
    spaces: [RecvdPackets; 3],
}

impl AckTracker {
    pub fn ack_time(&self) -> Option<Instant> {
        let mut iter = self.spaces.iter().filter_map(RecvdPackets::ack_time);
        match iter.next() {
            Some(v) => Some(iter.fold(v, min)),
            _ => None,
        }
    }

    pub fn acked(&mut self, token: &AckToken) {
        self.spaces[token.space as usize].acknowledged(&token.ranges);
    }

    /// Generate an ACK frame.
    ///
    /// Unlike other frame generators this doesn't modify the underlying instance
    /// to track what has been sent. This only clears the delayed ACK timer.
    ///
    /// When sending ACKs, we want to always send the most recent ranges,
    /// even if they have been sent in other packets.
    ///
    /// We don't send ranges that have been acknowledged, but they still need
    /// to be tracked so that duplicates can be detected.
    pub(crate) fn get_frame(
        &mut self,
        now: Instant,
        epoch: Epoch,
    ) -> Option<(Frame, Option<RecoveryToken>)> {
        let space = &mut self[PNSpace::from(epoch)];

        // Check that we aren't delaying ACKs.
        if !space.ack_now(now) {
            return None;
        }

        // Limit the number of ACK ranges we send so that we'll always
        // have space for data in packets.
        let ranges: Vec<PacketRange> = space
            .ranges
            .iter()
            .filter(|r| r.ack_needed())
            .take(MAX_ACKS_PER_FRAME)
            .cloned()
            .collect();
        let mut iter = ranges.iter();

        let first = match iter.next() {
            Some(v) => v,
            _ => return None, // Nothing to send.
        };
        let mut ack_ranges = Vec::new();
        let mut last = first.smallest;

        for range in iter {
            ack_ranges.push(AckRange {
                // the difference must be at least 2 because 0-length gaps,
                // (difference 1) are illegal.
                gap: last - range.largest - 2,
                range: range.len() - 1,
            });
            last = range.smallest;
        }

        // We've sent an ACK, reset the timer.
        space.ack_time = None;

        let ack_delay = now.duration_since(space.largest_pn_time.unwrap());
        // We use the default exponent so
        // ack_delay is in multiples of 8 microseconds.
        if let Ok(delay) = (ack_delay.as_micros() / 8).try_into() {
            let ack = Frame::Ack {
                largest_acknowledged: first.largest,
                ack_delay: delay,
                first_ack_range: first.len() - 1,
                ack_ranges,
            };
            Some((
                ack,
                Some(RecoveryToken::Ack(AckToken {
                    space: PNSpace::from(epoch),
                    ranges,
                })),
            ))
        } else {
            qwarn!(
                "ack_delay.as_micros() did not fit a u64 {:?}",
                ack_delay.as_micros()
            );
            None
        }
    }
}

impl Default for AckTracker {
    fn default() -> Self {
        Self {
            spaces: [
                RecvdPackets::new(PNSpace::Initial),
                RecvdPackets::new(PNSpace::Handshake),
                RecvdPackets::new(PNSpace::ApplicationData),
            ],
        }
    }
}

impl Index<PNSpace> for AckTracker {
    type Output = RecvdPackets;

    fn index(&self, space: PNSpace) -> &Self::Output {
        &self.spaces[space as usize]
    }
}

impl IndexMut<PNSpace> for AckTracker {
    fn index_mut(&mut self, space: PNSpace) -> &mut Self::Output {
        &mut self.spaces[space as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neqo_common::once::OnceResult;
    use std::collections::HashSet;

    fn now() -> Instant {
        static mut NOW_ONCE: OnceResult<Instant> = OnceResult::new();
        *unsafe { NOW_ONCE.call_once(Instant::now) }
    }

    fn test_ack_range(pns: &[u64], nranges: usize) {
        let mut rp = RecvdPackets::new(PNSpace::Initial); // Any space will do.
        let mut packets = HashSet::new();

        for pn in pns {
            rp.set_received(now(), *pn, true);
            packets.insert(*pn);
        }

        assert_eq!(rp.ranges.len(), nranges);

        // Check that all these packets will be detected as duplicates.
        for pn in pns {
            assert!(rp.is_duplicate(*pn));
        }

        // Check that the ranges decrease monotonically and don't overlap.
        let mut iter = rp.ranges.iter();
        let mut last = iter.next().expect("should have at least one");
        for n in iter {
            assert!(n.largest + 1 < last.smallest);
            last = n;
        }

        // Check that the ranges include the right values.
        let mut in_ranges = HashSet::new();
        for range in rp.ranges.iter() {
            for included in range.smallest..=range.largest {
                in_ranges.insert(included);
            }
        }
        assert_eq!(packets, in_ranges);
    }

    #[test]
    fn pn0() {
        test_ack_range(&[0], 1);
    }

    #[test]
    fn pn1() {
        test_ack_range(&[1], 1);
    }

    #[test]
    fn two_ranges() {
        test_ack_range(&[0, 1, 2, 5, 6, 7], 2);
    }

    #[test]
    fn fill_in_range() {
        test_ack_range(&[0, 1, 2, 5, 6, 7, 3, 4], 1);
    }

    #[test]
    fn too_many_ranges() {
        let mut rp = RecvdPackets::new(PNSpace::Initial); // Any space will do.

        // This will add one too many disjoint ranges.
        for i in 0..=MAX_TRACKED_RANGES {
            rp.set_received(now(), (i * 2) as u64, true);
        }

        assert_eq!(rp.ranges.len(), MAX_TRACKED_RANGES);
        assert_eq!(rp.ranges.back().unwrap().largest, 2);

        // Even though the range was dropped, we still consider it a duplicate.
        assert!(rp.is_duplicate(0));
        assert!(!rp.is_duplicate(1));
        assert!(rp.is_duplicate(2));
    }

    #[test]
    fn ack_delay() {
        // Only application data packets are delayed.
        let mut rp = RecvdPackets::new(PNSpace::ApplicationData);
        assert!(rp.ack_time().is_none());
        assert!(!rp.ack_now(now()));

        // One packet won't cause an ACK to be needed.
        rp.set_received(now(), 0, true);
        assert_eq!(Some(now() + ACK_DELAY), rp.ack_time());
        assert!(!rp.ack_now(now()));
        assert!(rp.ack_now(now() + ACK_DELAY));

        // A second packet will move the ACK time to now.
        rp.set_received(now(), 1, true);
        assert_eq!(Some(now()), rp.ack_time());
        assert!(rp.ack_now(now()));
    }

    #[test]
    fn no_ack_delay() {
        for space in &[PNSpace::Initial, PNSpace::Handshake] {
            let mut rp = RecvdPackets::new(*space);
            assert!(rp.ack_time().is_none());
            assert!(!rp.ack_now(now()));

            // Any packet will be acknowledged straight away.
            rp.set_received(now(), 0, true);
            assert_eq!(Some(now()), rp.ack_time());
            assert!(rp.ack_now(now()));
        }
    }

    #[test]
    fn aggregate_ack_time() {
        let mut tracker = AckTracker::default();
        // This packet won't trigger an ACK.
        tracker[PNSpace::Handshake].set_received(now(), 0, false);
        assert_eq!(None, tracker.ack_time());

        // This should be delayed.
        tracker[PNSpace::ApplicationData].set_received(now(), 0, true);
        assert_eq!(Some(now() + ACK_DELAY), tracker.ack_time());

        // This should move the time forward.
        let later = now() + ACK_DELAY.checked_div(2).unwrap();
        tracker[PNSpace::Initial].set_received(later, 0, true);
        assert_eq!(Some(later), tracker.ack_time());
    }
}
