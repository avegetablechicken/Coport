# Coport

Coport is a local proxy that routes coding agents' traffic through chosen upstreams, and a desktop app that shows that traffic across devices.

## Language

### Requests

**App**:
The coding agent a proxied request belongs to: Codex, Claude, Antigravity or Grok. Coport decides it once per request, from the request's target, and routes and records the request by it.
_Avoid_: service, client, provider

**Scoped target**:
A request target that names its **App** in a leading prefix, such as `/antigravity/…` or `/anthropic/…`. What follows the prefix is the target the **App** asked for.
_Avoid_: prefixed path, namespaced target

**Model call**:
A request that asks a model to generate output: a Responses, Messages, Chat Completions, Completions or Gemini generateContent call, or a Responses WebSocket connection.
_Avoid_: model request, inference

**Token refresh**:
A request that renews an **App**'s login at its OAuth token endpoint, whether a client sends it through Coport or Coport sends it itself.
_Avoid_: OAuth refresh, renewal request

**Auxiliary request**:
A request Coport sends upstream while serving a client's request that is not that request itself, such as an account lookup or a **Token refresh** made before retrying a **Model call**.
_Avoid_: side request, probe

**Request record**:
What Coport writes down about one request as it goes: received, routed, sent upstream and answered, then ended as finished, rejected, failed or cancelled. A **Model call**'s record also carries the call's ID; an **Auxiliary request** made for it does not.
_Avoid_: request log, log entry

**Activity Log**:
The app's list of **Request records** in a time range, newest first, filtered by kind and searched, a page at a time. While the range reaches the present it lists new records as they are written. It shows either what clients received or what upstreams returned.
_Avoid_: request log, history

**Route**:
The upstream a request is sent to, the proxy it goes through and the credential it carries, chosen by its **App** from the credential or the target the client sent.
_Avoid_: destination, path

**Saved login**:
An **App**'s OAuth login stored on this computer where the **App** keeps it. When a **Model call** carrying a Codex, Claude or Antigravity Saved login's access token is rejected as expired, Coport renews it with a **Token refresh** and sends the call again. Grok's own client renews Grok's; Coport relays that **Token refresh** and learns its account.
_Avoid_: credential file, stored token

**Account lookup**:
An **Auxiliary request** that asks an **App**'s upstream which account an access token belongs to. Its answer is remembered for that token, since a token's account never changes.
_Avoid_: profile lookup, userinfo

**Account discovery**:
**Account lookups** Coport makes in the background for the logins it knows, to show their account labels and **Route activation**. It runs only where the user names a proxy for it, and does not retry a failed lookup for a while.
_Avoid_: background probe

**Route activation**:
Whether an **App**'s account route has a login on this computer that it serves: active when local data says so, remote when only an **Account lookup** does, unknown while a login is unread or unidentified, probe failed when **Account discovery** failed for one, and inactive otherwise.
_Avoid_: route state, account state

### Running

**Daemon**:
The background process that runs the proxy for the desktop app on this computer. It outlives the app's window, and at most one runs per data directory.
_Avoid_: backend, service, helper

**Restart**:
Replacing the running **Daemon** with a new one. The new configuration and credentials are checked before the old one stops. Unless forced, the Restart waits for the old one's requests in flight. If the new one does not become healthy, the previous program runs again. One Restart runs at a time; another started meanwhile joins it.
_Avoid_: reload, relaunch

### Devices

**Device**:
Another computer the user adds, whose traffic Coport reads over HTTP or SSH. One reached over SSH can also be forwarded to.
_Avoid_: peer, node, remote, managed device

**Forwarding**:
Sending the requests this computer's proxy receives to one SSH **Device**, whose proxy answers them. It starts only once that **Device** was found to support it, stops when the **Device** is edited or removed, and can be restored when the **Daemon** starts.
_Avoid_: relay, tunnel, remote routing

**Summary**:
What a **Device** reports about its traffic for other devices to merge: its **Traffic totals** for each range and category, with accounts and upstreams named only by opaque references. A **Device** may send the requested range first and the rest after it.
_Avoid_: snapshot, export, peer data

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
