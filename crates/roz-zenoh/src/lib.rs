//! Zenoh local communication layer for roz edge robots.
//!
//! Provides peer-to-peer pub/sub for sensor data and motor commands
//! on the local robot network.

pub mod coordination;
pub mod pubsub;
pub mod session;
