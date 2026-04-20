//! UDP transport — `udpin:0.0.0.0:14540` (offboard) or `:14550` (GCS) per 25-CONTEXT.md.
//!
//! PX4 SITL footgun: copper BINDS `udpin:...`, PX4 BROADCASTS to that port.
//! See `docs/mavlink-coexistence.md` (plan 25-14) + 25-RESEARCH.md Pitfall 2.
//!
//! Wave 1 plan `25-06-transports` populates this file.
