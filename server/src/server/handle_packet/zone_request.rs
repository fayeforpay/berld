use protocol::packet::{AreaRequest, WorldUpdate};
use protocol::packet::area_request::Zone;

use crate::server::handle_packet::HandlePacket;
use crate::server::player::Player;
use crate::server::Server;

impl HandlePacket<AreaRequest<Zone>> for Server {
	async fn handle_packet(&self, source: &Player, packet: AreaRequest<Zone>) {
		let zone = packet.0;

		if let Some(blocks) = self.addons.models.packet_for(zone) {
			_ = source.send_raw(&blocks).await;
		}

		let loot_in_zone = self.loot
			.read().await
			.get(&zone)
			.cloned()
			.unwrap_or_default();

		// an entry for a zone (even an empty one) acknowledges discovery
		// and stops the client from re-requesting zones that remain loaded client-side
		// p48 is unsafe, so empty loot updates are sent for empty zones instead
		let acknowledgment = WorldUpdate {
			loot: [(zone, loot_in_zone)].into(),
			..Default::default()
		};

		source.send_ignoring(&acknowledgment).await;
	}
}