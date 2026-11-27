"use strict";

/// The Activity Log: Request records of one query (filter, response side,
/// search and time range), newest first, a page at a time. A newer query
/// wins over a slower older one. `show(update, live)` applies `update` and
/// redraws the list; a `live` update keeps the rows in view where they are.
/// `failed(error)` reports an older page that could not be read. `now()` is
/// the current minute.
function createActivityPager({ invoke, show, failed, now }) {
  const { createLatest } = typeof module !== "undefined" ? require("./latest.js") : globalThis;
  const reads = createLatest();
  const HOUR = 60 * 60 * 1000;
  /// The read of the latest older page, while it runs.
  let moreRead = null;

  const log = {
    /// requests, models, errors or all.
    filter: "requests",
    /// Lists what upstreams returned instead of what clients received.
    upstream: false,
    search: "",
    searchMode: "keyword",
    /// A preset re-resolved when chosen or the page opens, or "custom".
    /// Epoch milliseconds; `to` is an inclusive minute.
    preset: "hour",
    from: null,
    to: null,
    rows: [],
    /// The cursor of the next, older page, if any.
    next: null,
    /// Pages past the first have been loaded; a live update keeps them.
    paged: false,
    loadingMore: false,
    error: "",

    /// The backend filter. All Events lists both sides already.
    backendFilter() {
      return log.upstream && log.filter !== "all" ? `upstream_${log.filter}` : log.filter;
    },

    /// Sets the range from its preset, relative to the current minute.
    resolveRange() {
      if (log.preset === "custom" && log.to != null) return;
      const minute = now();
      const midnight = new Date(minute);
      midnight.setHours(0, 0, 0, 0);
      const yesterday = new Date(midnight);
      yesterday.setDate(yesterday.getDate() - 1);
      [log.from, log.to] = {
        "6h": [minute - 6 * HOUR, minute],
        today: [midnight.getTime(), minute],
        // `to` is an inclusive minute: the last minute before midnight.
        yesterday: [yesterday.getTime(), midnight.getTime() - 60 * 1000],
      }[log.preset] ?? [minute - HOUR, minute];
    },

    /// Lists the range newest first. `"more"` appends the next, older page;
    /// `"live"` re-reads the log for lines written since and keeps the older
    /// pages already loaded. Otherwise the backend reuses its last read of the
    /// range for filters, searches and pages.
    async load(mode) {
      if (log.to == null) log.resolveRange();
      const query = {
        filter: log.backendFilter(), search: log.search, searchMode: log.searchMode,
        range: { from: log.from, to: log.to + 60 * 1000 },
        after: mode === "more" ? log.next : null,
        fresh: mode === "live",
      };
      // A live refresh must not invalidate a scan of the same query still running.
      const key = JSON.stringify([query.filter, query.search, query.searchMode, query.range, query.after]);
      if (mode === "live" && reads.loading(key)) return;
      const read = reads.begin(key);
      const current = () => read.current() && query.filter === log.backendFilter() && query.search === log.search
        && query.searchMode === log.searchMode && query.range.from === log.from && query.range.to === log.to + 60 * 1000;
      // The latest older page's read ends its loading, whatever live read
      // follows. Another query replaces the pages that one was adding to.
      if (mode === "more") { log.loadingMore = true; moreRead = read; }
      else if (mode !== "live") { log.loadingMore = false; moreRead = null; }
      let activity;
      try {
        activity = await invoke("get_activity", query);
      } catch (error) {
        // A failed live update keeps the list; the next one retries.
        if (!current() || mode === "live") return;
        // A failed older page keeps the pages already loaded, so it can be retried.
        if (mode === "more") {
          show(() => {});
          failed(error);
          return;
        }
        show(() => {
          log.rows = [];
          log.next = null;
          log.error = String(error);
        });
        return;
      } finally {
        read.done();
        if (moreRead === read) { log.loadingMore = false; moreRead = null; }
      }
      if (!current()) return;
      log.error = "";
      if (mode === "more") {
        show(() => {
          log.rows = log.rows.concat(activity.rows);
          log.next = activity.next;
          log.paged = true;
        });
        return;
      }
      // Keep older pages only when the new page reaches the already loaded rows.
      // Otherwise a burst larger than one page would leave an unpageable gap.
      const last = activity.rows.at(-1);
      const overlaps = last && log.rows.some((e) => e.time === last.time && e.seq === last.seq);
      const older = mode === "live" && log.paged && activity.next && overlaps
        ? log.rows.filter((e) => e.time >= log.from && (e.time < last.time || (e.time === last.time && e.seq < last.seq)))
        : [];
      show(() => {
        log.rows = activity.rows.concat(older);
        if (!older.length) log.next = activity.next;
        log.paged = older.length > 0;
      }, mode === "live");
    },

    /// Lists lines written since the last read when the range reaches the
    /// present, or a preset moved with the clock.
    refresh() {
      const [from, to] = [log.from, log.to];
      log.resolveRange();
      if (from === log.from && to === log.to && to < now()) return;
      return log.load("live");
    },
  };
  return log;
}

if (typeof module !== "undefined") module.exports = { createActivityPager };
