use std::collections::{HashMap, HashSet};
use std::f64::consts::PI;

use config::{Config, ConfigError};
use serde::de::DeserializeOwned;
use strum::IntoEnumIterator;
use tap::Tap;
use tokio::sync::RwLock;

use protocol::nalgebra::Point3;
use protocol::packet::common::*;
use protocol::packet::common::item::*;
use protocol::packet::creature_update::*;
use protocol::packet::creature_update::equipment::Slot;
use protocol::packet::world_update::{Pickup, sound};
use protocol::packet::WorldUpdate;
use protocol::packet::CreatureUpdate;
use protocol::utils::constants::{materials, SIZE_BLOCK, SIZE_ZONE};
use protocol::utils::constants::rarity::*;
use protocol::utils::flagset::FlagSet;

use crate::addon::play_sound_at_player;
use crate::server::player::Player;
use crate::server::Server;

const SHOP_CENTER: Point3<i64> = Point3::new(550361514520, 550354653448, 5498473);
const SHOP_RADIUS: i64 = SIZE_BLOCK * 15;
const SHOP_INDEX: i32 = 1000;
const SHOP_ID: i64 = 100000;
const KEEPER_INDEX: i32 = 2000;
const KEEPER_ID: i64 = 200000;
const DISABLED_ITEMS: [Kind; 2] = [Kind::PlatinumCoin, Kind::ManaCube];

const NAME_OVERFLOW: usize = 15;

#[derive(Default, Clone, Copy)]
pub enum ShopState {
    #[default]
    MainType,
    SubType,
    Material,
    Rarity,
    Seed,
    Complete,
}

#[derive(Default)]
pub struct ShopSession {
    pub state: ShopState,
    pub item: Item,
    pub npcs: Vec<CreatureUpdate>
}

pub struct ItemShop {
    pub shop_center: Point3<i64>,
    pub shop_radius: i64,
    pub sessions: RwLock<HashMap<CreatureId, ShopSession>>,
}

impl ItemShop {
    pub fn new(config: &Config) -> Result<Self, ConfigError> {
        Ok(Self {
            shop_center: config_fallback(config, "shop.center", SHOP_CENTER)?,
            shop_radius: config_fallback(config, "shop.radius", SHOP_RADIUS)?,
            sessions: RwLock::new(HashMap::new())
        })
    }

    pub async fn shop_keeper(&self, player: &Player) {
        let shop_keeper = CreatureUpdate {
            appearance: Some(Appearance { body_model: 2111, ..appearance_template() }),
            id: CreatureId(KEEPER_ID),
            race: Some(Race::Bandit),
            name: Some("Item\nShop".into()),
            position: Some(self.shop_center),
            zone_data_index: Some(Point3::new(
                (self.shop_center.x / SIZE_ZONE) as i32,
                (self.shop_center.y / SIZE_ZONE) as i32,
                KEEPER_INDEX)),
            ..Default::default()
        };

        player.send_ignoring(&shop_keeper).await
    }

    pub async fn interaction(&self, server: &Server, player: &Player, index: Point3<i32>) {
        self.cleanup_stale_sessions(server).await;

        if index == Point3::new((self.shop_center.x / SIZE_ZONE) as i32, (self.shop_center.y / SIZE_ZONE) as i32, KEEPER_INDEX) {
            play_sound_at_player(player, sound::Kind::CraftProc, 1.0, 1.0).await;
            self.reset_session(player).await;
            self.update_shop(player).await;
            return
        }

        let is_shop_npc = self.sessions
            .read()
            .await
            .get(&player.id)
            .is_some_and(|s| s.npcs.iter().any(|c| c.zone_data_index == Some(index)));

        if !is_shop_npc {
            return
        }

        let option = index.z - SHOP_INDEX;
        let player_level = player.character.read().await.level as i16;

        let item_state: Result<Option<Box<Item>>, &'static str> = {
            let mut sessions = self.sessions.write().await;
            let session = sessions.entry(player.id).or_default();
            item_selection(&mut session.state, &mut session.item, option);
            item_validation(&mut session.state, &mut session.item, player_level)
                .map(|()| matches!(session.state, ShopState::Complete).then(|| Box::new(session.item.clone())))
        };

        match item_state {
            Err(reason)    => {play_sound_at_player(player, sound::Kind::SpikeTrap, 1.0, 1.0).await;
                               player.notify(reason).await;
                               self.reset_session(player).await}
            Ok(Some(item)) => {play_sound_at_player(player, sound::Kind::DropCoin, 1.0, 1.0).await;
                               let pickup = Pickup { interactor: player.id, item: *item };
                               player.send_ignoring(&WorldUpdate::from(pickup)).await;
                               self.reset_session(player).await}
            Ok(None)       =>  play_sound_at_player(player, sound::Kind::Craft, 1.0, 1.0).await
        }

        self.update_shop(player).await
    }

    async fn reset_session(&self, player: &Player) {
        let mut sessions = self.sessions.write().await;
        let session = sessions.entry(player.id).or_default();
        session.state = ShopState::default();
        session.item = Item::default()
    }

    async fn cleanup_stale_sessions(&self, server: &Server) {
        let online_ids: HashSet<CreatureId> = server.players
            .read()
            .await
            .iter()
            .map(|p| p.id)
            .collect();

        self.sessions.write().await.retain(|id, _| online_ids.contains(id))
    }

    async fn update_shop(&self, player: &Player) {
        let (old_npcs, new_npcs) = {
            let mut sessions = self.sessions.write().await;
            let session = sessions.entry(player.id).or_default();

            let old_npcs = std::mem::take(&mut session.npcs);
            let options = shop_options(&session.state, &session.item);
            let new_npcs = shop_npcs(self.shop_center, self.shop_radius, &options, &session.state, &session.item);
            session.npcs = new_npcs.clone();

            (old_npcs, new_npcs)
        };

        for packet in &old_npcs {
            player.send_ignoring(&CreatureUpdate {
                id: packet.id,
                health: Some(0.0),
                ..Default::default()
            }).await
        }

        for packet in &new_npcs {
            player.send_ignoring(packet).await
        }
    }
}

fn config_fallback<T: DeserializeOwned>(config: &Config, key: &str, default: T) -> Result<T, ConfigError> {
    match config.get(key) {
        Ok(value)                     => Ok(value),
        Err(ConfigError::NotFound(_)) => Ok(default),
        Err(err)                      => Err(err)
    }
}

fn appearance_template() -> Appearance {
    Appearance {
        flags: FlagSet::default().tap_mut(|fs| {
            fs.set(AppearanceFlag::Unknown7, true);
            fs.set(AppearanceFlag::Immovable, true);
        }),
        creature_size: Hitbox {
            width: 1.5,
            depth: 1.5,
            height: 2.5
        },
        head_model    : -1,
        hair_model    : -1,
        hand_model    : -1,
        foot_model    : -1,
        body_model    : -1,
        tail_model    : -1,
        shoulder2model: -1,
        wing_model    : -1,
        body_size     : 1.0,
        ..Default::default()
    }
}

fn shop_npcs(center: Point3<i64>, radius: i64, options: &[i32], state: &ShopState, item: &Item) -> Vec<CreatureUpdate> {
    options
        .iter()
        .enumerate()
        .map(|(i, &option)| {
            let angle = 2.0 * PI * (i as f64) / (options.len() as f64);
            let x = (center.x as f64) + (radius as f64) * angle.cos();
            let y = (center.y as f64) + (radius as f64) * angle.sin();

            const YAW_OFFSET: f64 = 90.0;
            let dx = center.x as f64 - x;
            let dy = center.y as f64 - y;
            let yaw = (dy.atan2(dx) * 180.0 / PI - YAW_OFFSET) as f32;

            let preview = item_preview(*state, item, option).unwrap_or_else(|| item.clone());
            let mut equipment = Equipment::default();
            equipment[Slot::Chest] = preview;

            CreatureUpdate {
                appearance: Some(appearance_template().tap_mut(|a| {
                    a.body_model = 2316; 
                    a.body_offset.z = 5.0
                })),
                id: CreatureId(SHOP_ID + i as i64),
                race: Some(Race::Bandit),
                name: Some(npc_names(*state, item, option).chars().take(NAME_OVERFLOW).collect()),
                health: Some(0.0001),
                rotation: Some(EulerAngles { pitch: 0.0, roll: 0.0, yaw }),
                position: Some(Point3::new(x.round() as i64, y.round() as i64, center.z)),
                zone_data_index: Some(Point3::new(
                    (x.round() as i64 / SIZE_ZONE) as i32,
                    (y.round() as i64 / SIZE_ZONE) as i32,
                    SHOP_INDEX + option)),
                equipment: Some(equipment),
                ..Default::default()
            }
        })
        .collect()
}

fn maintypes(option: i32) -> Option<Kind> {
    Kind::iter().find(|k| KindDiscriminants::from(*k) as u8 == option as u8)
}

fn subtypes(kind: &Kind) -> Vec<(i32, Kind)> {
    match kind {
        Kind::Consumable(_) => kind::Consumable::iter().map(|s| (s as i32, Kind::Consumable(s))).collect(),
        Kind::Weapon(_)     => kind::Weapon::iter().map(|s| (s as i32, Kind::Weapon(s))).collect(),
        Kind::Resource(_)   => kind::Resource::iter().map(|s| (s as i32, Kind::Resource(s))).collect(),
        Kind::Candle(_)     => kind::Candle::iter().map(|s| (s as i32, Kind::Candle(s))).collect(),
        Kind::Pet(_)        => Race::iter().map(|s| (s as i32, Kind::Pet(s))).collect(),
        Kind::PetFood(_)    => Race::iter().map(|s| (s as i32, Kind::PetFood(s))).collect(),
        Kind::Quest(_)      => kind::Quest::iter().map(|s| (s as i32, Kind::Quest(s))).collect(),
        Kind::Special(_)    => kind::Special::iter().map(|s| (s as i32, Kind::Special(s))).collect(),
        _                   => Vec::new()
    }
}

fn shop_options(state: &ShopState, item: &Item) -> Vec<i32> {
    match state {
        ShopState::MainType => Kind::iter().map(|k| KindDiscriminants::from(k) as i32).collect(),
        ShopState::SubType  => subtypes(&item.kind).into_iter().map(|(s, _)| s).collect(),
        ShopState::Material => materials::by_item_kind(item.kind).iter().map(|&m| m as i32).collect(),
        ShopState::Rarity   => [NORMAL, UNCOMMON, RARE, EPIC, LEGENDARY].iter().map(|&r| r as i32).collect(),
        ShopState::Seed     => (-10..=10).collect(),
        ShopState::Complete => Vec::new()
    }
}

fn item_selection(state: &mut ShopState, item: &mut Item, option: i32) {
    if let Some(preview) = item_preview(*state, item, option) {
        *item = preview;
        *state = match *state {
            ShopState::MainType => ShopState::SubType,
            ShopState::SubType  => ShopState::Material,
            ShopState::Material => ShopState::Rarity,
            ShopState::Rarity   => ShopState::Seed,
            ShopState::Seed     => ShopState::Complete,
            ShopState::Complete => ShopState::Complete
        }
    }
}

fn item_preview(state: ShopState, item: &Item, option: i32) -> Option<Item> {
    let mut preview = item.clone();
    match state {
        ShopState::MainType => {preview.kind = maintypes(option)?}
        ShopState::SubType  => {let (_, kind) = subtypes(&item.kind).into_iter().find(|(o, _)| *o == option)?;
                                preview.kind = kind}
        ShopState::Material => {preview.material = Material::from_repr(option as i8)?}
        ShopState::Rarity   => {preview.rarity = option as u8}
        ShopState::Seed     => {if !(-10..=10).contains(&option) { return None }
                                preview.seed = option}
        ShopState::Complete => return None
    }
    Some(preview)
}

fn item_validation(state: &mut ShopState, item: &mut Item, player_level: i16) -> Result<(), &'static str> {
    if DISABLED_ITEMS.contains(&item.kind) { return Err("this item is currently disabled") }
    loop {
        match *state {
            ShopState::MainType => return Ok(()),
            ShopState::SubType  => {if subtypes(&item.kind).is_empty() { *state = ShopState::Material }
                                    else { return Ok(()) }}
            ShopState::Material => {let valid_materials = materials::by_item_kind(item.kind);
                                    match valid_materials.len() {
                                        0 => return Err("item has no valid materials"),
                                        1 => {item.material = valid_materials[0];
                                             *state = ShopState::Rarity}
                                        _ => return Ok(())}}
            ShopState::Rarity   => {if item.kind.uses_rarity() { return Ok(()) }
                                    item.rarity = NORMAL;
                                    *state = ShopState::Seed}
            ShopState::Seed     => {if item.kind.uses_seed() { return Ok(()) }
                                    item.seed = 0;
                                    *state = ShopState::Complete}
            ShopState::Complete => {item.level = if item.kind.uses_level() { player_level } else { 1 };
                                    return Ok(())}
        }
    }
}

fn subtype_names(kind: &Kind) -> String {
    match kind {
        Kind::Consumable(s)  => s.to_string(),
        Kind::Weapon(s)      => s.to_string(),
        Kind::Resource(s)    => s.to_string(),
        Kind::Candle(s)      => s.to_string(),
        Kind::Pet(s)         => s.to_string(),
        Kind::PetFood(s)     => s.to_string(),
        Kind::Quest(s)       => s.to_string(),
        Kind::Special(s)     => s.to_string(),
        _                    => String::new()
    }
}

fn rarity_names(option: i32) -> &'static str {
    match option as u8 {
        NORMAL    => "Normal",
        UNCOMMON  => "Uncommon",
        RARE      => "Rare",
        EPIC      => "Epic",
        LEGENDARY => "Legendary",
        _         => ""
    }
}

fn npc_names(state: ShopState, item: &Item, option: i32) -> String {
    match state {
        ShopState::MainType => maintypes(option).map(|k| k.to_string()).unwrap_or_default(),
        ShopState::SubType  => subtypes(&item.kind)
            .into_iter()
            .find(|(o, _)| *o == option)
            .map(|(_, s)| subtype_names(&s))
            .unwrap_or_default(),
        ShopState::Material => Material::from_repr(option as i8).map(|m| m.to_string()).unwrap_or_default(),
        ShopState::Rarity   => rarity_names(option).to_string(),
        ShopState::Seed     => option.to_string(),
        ShopState::Complete => String::new()
    }
}