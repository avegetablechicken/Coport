"use strict";

/// Latest request wins. Each read takes a ticket that every later ticket
/// supersedes, so a slower, older read never overwrites a newer one. A read
/// still in flight can be found by its key and joined instead of repeated.
function createLatest() {
  let issued = 0;
  let applied = 0;
  let loadingKey = null;
  let loadingTicket = 0;
  return {
    /// Whether the newest ticket's read, of `key`, is still in flight.
    loading(key) {
      return loadingKey === key && loadingTicket === issued;
    },
    /// A ticket for a read of `key`, superseding every earlier one.
    begin(key = null) {
      const ticket = ++issued;
      loadingKey = key;
      loadingTicket = ticket;
      return {
        /// No later ticket was issued.
        current: () => ticket === issued,
        /// No later ticket's result was applied yet; marks this one applied.
        /// For reads whose results may land out of order but never go back.
        apply() {
          if (ticket < applied) return false;
          applied = ticket;
          return true;
        },
        /// This ticket's read ended.
        done() {
          if (loadingTicket === ticket) loadingKey = null;
        },
      };
    },
    /// Supersedes every ticket issued so far.
    cancel() {
      ++issued;
    },
  };
}

if (typeof module !== "undefined") module.exports = { createLatest };
