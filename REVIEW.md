# Pending

Remaining findings from the repository-wide code health pass (2026-08-17). The other
findings from that pass have since been fixed; see SPECS.md for the decisions taken.

## `src/tui/mod.rs` has no test module

README.md asks every file in `src/` to end with its own test module. `src/tui/mod.rs`
(the terminal event loop) has none, and it is the one remaining file where that is a
real gap rather than a formality — `src/lib.rs` is only `pub mod` declarations.

The loop mutates `App` in response to `crossterm` events, so testing it needs a way to
feed synthetic events without a terminal. That is a test-harness change rather than a
unit test, which is why it was not done as part of a health pass. Everything the loop
*decides* is currently untested: which key maps to which view, that Escape cancels
rather than commits a rename, that the theme toggle persists.

## Poll intervals are read once at spawn time

A poll loop reads `poll_interval_secs` from its `DeviceRecord` when it starts and builds
its ticker from it. Changing a device's interval in the DB therefore has no effect until
the process restarts. Nothing in the UI currently edits intervals, so this is latent
rather than broken — worth knowing before adding that.

## `resolve_redirect` cannot follow a redirect to an IPv6 literal

The host/port split uses `rfind(':')`. For `http://[::1]:8080/x` that does pick the right
port, but the host comes out as `[::1]`, which `IpAddr::from_str` rejects (it does not
accept brackets) — so the redirect is followed against the address already being probed
instead of the one named. A bare IPv6 authority is misread outright.

Harmless today: local devices redirect to IPv4 addresses or to hostnames, and the
fall-back-to-the-probed-address behaviour is deliberate for hostnames. It would matter for
a device that redirects across hosts on an IPv6-only segment.
