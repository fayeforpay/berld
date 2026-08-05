use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::nalgebra::Point2;
use protocol::packet::{AreaRequest, WorldUpdate};
use protocol::packet::area_request::Zone;
use tokio::time::sleep;

use crate::server::handle_packet::HandlePacket;
use crate::server::player::addon_data::ZoneState;
use crate::server::player::Player;
use crate::server::Server;
use crate::SERVER;

const CENTER_SETTLE: Duration = Duration::from_secs(1);
const STALE_RETRY_GRACE: Duration = Duration::from_secs(2);

impl HandlePacket<AreaRequest<Zone>> for Server {
	async fn handle_packet(&self, source: &Player, packet: AreaRequest<Zone>) {
		let zone = packet.0;

		let Some(player) = self.find_player_by_id(source.id).await
		else { return };

		self.answer_request(&player, zone).await;
	}
}

impl Server {
	// client's zone request proves lack of content, no matter what the server believes
	// it always gets answered, otherwise client re-requests forever
	async fn answer_request(&self, player: &Arc<Player>, zone: Point2<i32>) {
		{
			let mut addon_data = player.addon_data.write().await;
			match addon_data.zone_states.get(&zone) {
				Some(ZoneState::Pending) => return,
				Some(ZoneState::Revealed(at)) if at.elapsed() < STALE_RETRY_GRACE => return,
				_ => {}
			}
			addon_data.zone_states.insert(zone, ZoneState::Pending);
		}

		// loot alone is safe immediately
		let settle = if self.addons.models.packet_for(zone).is_some() {
			CENTER_SETTLE
		} else {
			Duration::ZERO
		};

		let player = Arc::clone(player);
		tokio::spawn(async move {
			SERVER.deliver_zone(&player, zone, settle).await;
		});
	}

	// everything waits out the settle, not just terrain: acknowledgment itself is zone-keyed
	// client ignores zone update if sent too early (before client-side zone generation)
	async fn deliver_zone(&self, player: &Arc<Player>, zone: Point2<i32>, settle: Duration) {
		sleep(settle).await;

		let delivered = self.send_zone(player, zone).await;

		let mut addon_data = player.addon_data.write().await;
		if delivered {
			addon_data.zone_states.insert(zone, ZoneState::Revealed(Instant::now()));
		} else {
			// let a later reveal or request retry it
			addon_data.zone_states.remove(&zone);
		}
	}

	async fn send_zone(&self, player: &Arc<Player>, zone: Point2<i32>) -> bool {
		// a zone change that took this zone out of range drops the claim
		if !player.is_near(zone).await {
			return false;
		}

		if let Some(blocks) = self.addons.models.packet_for(zone)
			&& player.send_raw(&blocks).await.is_err()
		{
			return false;
		}

		self.acknowledge(player, zone).await;
		true
	}

	// an entry for a zone (even an empty one) acknowledges discovery
	// and stops the client from re-requesting zones that remain loaded client-side
	// p48 is unsafe, so empty loot updates are sent for empty zones instead
	async fn acknowledge(&self, player: &Player, zone: Point2<i32>) {
		let loot_in_zone = self.loot
			.read().await
			.get(&zone)
			.cloned()
			.unwrap_or_default();

		let acknowledgment = WorldUpdate {
			loot: [(zone, loot_in_zone)].into(),
			..Default::default()
		};
		player.send_ignoring(&acknowledgment).await;
	}
}