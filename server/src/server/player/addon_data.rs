use std::collections::HashMap;
use std::time::Instant;

use protocol::nalgebra::Point2;

use crate::addon::anti_cheat::PlayerData;

#[derive(Debug, Default)]
pub struct AddonData {
	pub team: Option<i32>,
	pub anti_cheat_data: PlayerData,
	pub last_attacker: Option<(Instant, String)>,
	pub zone_states: HashMap<Point2<i32>, ZoneState>
}

#[derive(Debug, Clone, Copy)]
pub enum ZoneState {
	Pending,
	Revealed(Instant)
}