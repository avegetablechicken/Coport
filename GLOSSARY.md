# Coport

Coport is a local proxy that routes coding agents' traffic through chosen upstreams, and a desktop app that shows that traffic across devices.

## Language

### Traffic

**Traffic totals**:
The raw counts a device records for a stretch of time: requests, errors, bytes, tokens, timing sums with their sample counts, and per-bucket counts. Totals from different buckets, accounts or devices add up exactly.
_Avoid_: stats, raw stats, counters

**Traffic metrics**:
Rates and averages derived from **Traffic totals**: Avg. Time, **First Token**, **Output TPS**, cache hit rate and error rate. They are never added together; they are derived once, after the totals are summed.
_Avoid_: derived stats, computed fields

**Device traffic**:
The **Traffic totals** and **Traffic metrics** of one device for a time range: this device's own, the forwarded device's, or a peer's.
_Avoid_: device view, card data

**Merged Traffic**:
The sum of every device's **Device traffic**, each device counted exactly once and their buckets lined up by time.
_Avoid_: total traffic, combined traffic

**First Token**:
The average time until a successful model call streams its first output. Each call is timed from its first upstream send when that is known, otherwise from the request's start.
_Avoid_: TTFT, first byte

**Output TPS**:
Output tokens per second of a model call's streaming, timed from its first streamed token. Its variant timed from the request's start is **Output TPS from request start**.
_Avoid_: tokens per second, throughput
