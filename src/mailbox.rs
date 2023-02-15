//! # Mailbox module
//!
//! The mailbox module defines the `MavMailbox` which is responsible for storing the
//! `MAILBOX_MAX_SIZE` most recent messages of each type. If the message limit is
//! exceeded, the oldest message is droped in favor of the new one. Users of the
//! mailbox may can read the `next` message in arrival order with the next message with
//! a particular message id (`next_with_id`).

use std::{
    collections::{HashMap, VecDeque},
    time::Instant,
};

use mavlink::{Message, MavFrame};
use tokio::sync::{Mutex, Semaphore, SemaphorePermit};

// The number of entries to keep of a single message id
pub const MAILBOX_MAX_SIZE: usize = 16;

// TODO: Optimize `HashMap` of `u32`
/// The mailbox stores only the most recent message of a given `message_id` (the key of the map).
/// If a new message arrives and there is no space, it will push back the new one and remove the
/// old one.
pub(crate) struct MavMailbox<M: Message> {
    // The number of permits is the total number of entries in the mailbox.
    sem:   Semaphore,
    // TODO: I think a better implementation would use `std::sync::{Mutex, Condvar}`.
    inner: Mutex<MailboxInner<M>>,
}

/// The queue stores messages in order of arrival for a given id. There is an intrusive doubly
/// linked list with head `first_msg` and tail `last_msg` that stores the order of arrival across
/// all ids, this way we can traverse the mailbox both for each message id as well as overall
/// arrival order.
struct MailboxInner<M: Message> {
    entries:   HashMap<u32, VecDeque<MailboxEntry<M>>>,
    last_msg:  Option<(u32, Instant)>,
    first_msg: Option<(u32, Instant)>,
}

// FIXME: `Instant` only guarantees that the times are monotonically nondecreasing not increasing,
// and thus no less than previously measured times. Thus there is a chance that two messages would
// be inserted at the same time. This would brake the "unique link" requirement for the intrusive
// linked list.
struct MailboxEntry<M: Message> {
    frame:   MavFrame<M>,
    arrival: Instant,
    // TODO: I think `Instant` here is not necessary. We already know that the next message comes
    // after the current one. So if we just indicated the `id` of the next message, this should be
    // enough (the next message would have the indicated `id` and be the upper bound of the current
    // instant). This could be fixed using the extra byte in the message id to store a "generation
    // id" that would only increment when there are conflicts. This would allow up to 256
    // insertions with the same `Instant` which should be enough.
    prev:    Option<(u32, Instant)>,
    next:    Option<(u32, Instant)>,
}

impl<M: Message> MavMailbox<M> {
    pub(crate) fn new() -> Self {
        MavMailbox {
            sem:   Semaphore::new(0),
            inner: Mutex::new(MailboxInner::new()),
        }
    }

    // pub(crate) fn len(&self) -> usize {
    //     self.sem.available_permits()
    // }

    pub(crate) fn close(&self) {
        self.sem.close();
    }

    pub(crate) async fn insert_now(&self, pair: MavFrame<M>) {
        let Self { inner, sem } = self;
        inner.lock().await.insert_now(pair, sem);
    }

    pub(crate) async fn next(&self) -> Option<MavFrame<M>> {
        let permit = self.sem.acquire().await.ok()?;
        let mut locked = self.inner.lock().await;

        // It is ok to `unwrap` here since we know that there is at least one item in the mailbox
        // since we have acquired a permit.
        let (id, instant) = locked.first_msg.unwrap();

        // The `first_msg` has `id`, and we got a permit, so there must be an entry with `id` in
        // the mailbox.
        let entry = locked.pop_front_id(id, permit).unwrap();
        assert_eq!(entry.arrival, instant);
        Some(entry.frame)
    }

    pub(crate) async fn next_with_id(&self, id: u32) -> Option<MavFrame<M>> {
        loop {
            let permit = self.sem.acquire().await.ok()?;
            let mut locked = self.inner.lock().await;
            if let Some(next) = locked.pop_front_id(id, permit) {
                return Some(next.frame);
            }
            // In case the item that just arrived wasn't the one that we wanted, keep waiting.
            // One important side effect of this is that this future will be notified for any
            // and every message arriving, not just the ones with the correct id.
        }
    }
}

impl<M: Message> MailboxInner<M> {
    fn new() -> Self {
        MailboxInner {
            entries:   HashMap::new(),
            last_msg:  None,
            first_msg: None,
        }
    }

    fn insert_now(&mut self, frame: MavFrame<M>, sem: &Semaphore) {
        let arrival = Instant::now();
        let id = frame.msg.message_id();
        let deque = self.entries.entry(id).or_insert(VecDeque::new());
        let entry = MailboxEntry {
            frame,
            arrival,
            prev: self.last_msg,
            next: None,
        };
        let entry_idx = (id, arrival);
        deque.push_back(entry);
        sem.add_permits(1);
        if deque.len() > MAILBOX_MAX_SIZE {
            // We have just added a new permit and we have unique access (&mut) to the mailbox.
            let permit = sem.try_acquire().unwrap();
            self.pop_front_id(id, permit);
        }

        // Update the `prev`ious entry to have it's `next` pointing to the newly added entry.
        if let Some(idx) = self.last_msg {
            self.get_entry_mut(idx).next.replace(entry_idx);
        }
        self.last_msg.replace(entry_idx);
        if self.first_msg.is_none() {
            self.first_msg.replace(entry_idx);
        }
    }

    // The invariant of this function is that it should only be called with valid indicies.
    fn get_entry_mut(&mut self, idx: (u32, Instant)) -> &mut MailboxEntry<M> {
        self.entries
            .get_mut(&idx.0)
            .and_then(|deque| {
                deque
                    .binary_search_by_key(&idx.1, |el| el.arrival)
                    .map(|idx| &mut deque[idx])
                    .ok()
            })
            .expect("`get_entry_mut` was called with an invalid index")
    }

    // The `permit` means that there is a proof that there must be at least one item in the
    // mailbox. However, it may not have the same id as `id`. If the item is popped, then the
    // permit is forgotten, which marks the element as removed from the mailbox. If the item is not
    // consumend, then the permit will be dropped which will release the semaphore, sigaling that
    // no item was consumed.
    fn pop_front_id(&mut self, id: u32, permit: SemaphorePermit) -> Option<MailboxEntry<M>> {
        let deque = self.entries.get_mut(&id)?;
        let entry = deque.pop_front()?;
        let entry_idx = (entry.frame.msg.message_id(), entry.arrival);
        if let Some(prev) = entry.prev {
            self.get_entry_mut(prev).next = entry.next;
        } else {
            assert_eq!(self.first_msg, Some(entry_idx));
            self.first_msg = entry.next;
        }
        if let Some(next) = entry.next {
            self.get_entry_mut(next).prev = entry.prev;
        } else {
            assert_eq!(self.last_msg, Some(entry_idx));
            self.last_msg = entry.prev;
        }
        permit.forget();
        Some(entry)
    }

    // fn peek_front_id(&self, id: u32) -> Option<&MailboxEntry> {
    //     self.entries.get(&id).and_then(|deque| deque.front())
    // }
}
