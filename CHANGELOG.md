Changelog](https://keepachangelog.com/); versions follow [semver](https://semver.org).
- `tests/config_knobs.rs`: config-knob behavior matrix; default-vs-configured
  the hot paths; `put_miss`, `put_hit` (dedup), `get_cold_verified`,
  (the hex-path plumbing); no compression buffer, no re-store; turning
  live set (objects are opaque blobs, so roots *are* the live set; the