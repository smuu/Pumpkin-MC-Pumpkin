use std::{collections::HashMap, sync::Arc};

pub mod level_time;
pub mod player_chunker;

use crate::{
    command::client_cmd_suggestions,
    entity::{living::LivingEntity, player::Player, Entity},
    error::PumpkinError,
    server::Server,
};
use level_time::LevelTime;
use pumpkin_config::BasicConfiguration;
use pumpkin_core::math::vector2::Vector2;
use pumpkin_core::math::{position::WorldPosition, vector3::Vector3};
use pumpkin_core::text::{color::NamedColor, TextComponent};
use pumpkin_entity::{entity_type::EntityType, pose::EntityPose, EntityId};
use pumpkin_protocol::{
    client::play::CLevelEvent,
    codec::{identifier::Identifier, var_int::VarInt},
};
use pumpkin_protocol::{
    client::play::{CBlockUpdate, CRespawn, CSoundEffect, CWorldEvent},
    SoundCategory,
};
use pumpkin_protocol::{
    client::play::{
        CChunkData, CGameEvent, CLogin, CPlayerInfoUpdate, CRemoveEntities, CRemovePlayerInfo,
        CSetEntityMetadata, CSpawnEntity, GameEvent, Metadata, PlayerAction,
    },
    ClientPacket,
};
use pumpkin_registry::DimensionType;
use pumpkin_world::chunk::ChunkData;
use pumpkin_world::level::Level;
use pumpkin_world::{
    block::block_registry::{
        get_block_and_state_by_state_id, get_block_by_state_id, get_state_by_state_id,
    },
    coordinates::ChunkRelativeBlockCoordinates,
};
use rand::{thread_rng, Rng};
use scoreboard::Scoreboard;
use thiserror::Error;
use tokio::sync::{mpsc::Receiver, Mutex};
use tokio::{
    runtime::Handle,
    sync::{mpsc, RwLock},
};
use worldborder::Worldborder;

pub mod bossbar;
pub mod custom_bossbar;
pub mod scoreboard;
pub mod worldborder;

#[derive(Debug, Error)]
pub enum GetBlockError {
    BlockOutOfWorldBounds,
    InvalidBlockId,
}

impl std::fmt::Display for GetBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl PumpkinError for GetBlockError {
    fn is_kick(&self) -> bool {
        false
    }

    fn severity(&self) -> log::Level {
        log::Level::Warn
    }

    fn client_kick_reason(&self) -> Option<String> {
        None
    }
}

/// Represents a Minecraft world, containing entities, players, and the underlying level data.
///
/// Each dimension (Overworld, Nether, End) typically has its own `World`.
///
/// **Key Responsibilities:**
///
/// - Manages the `Level` instance for handling chunk-related operations.
/// - Stores and tracks active `Player` entities within the world.
/// - Provides a central hub for interacting with the world's entities and environment.
pub struct World {
    /// The underlying level, responsible for chunk management and terrain generation.
    pub level: Arc<Level>,
    /// A map of active players within the world, keyed by their unique UUID.
    pub current_players: Arc<Mutex<HashMap<uuid::Uuid, Arc<Player>>>>,
    /// The world's scoreboard, used for tracking scores, objectives, and display information.
    pub scoreboard: Mutex<Scoreboard>,
    /// The world's worldborder, defining the playable area and controlling its expansion or contraction.
    pub worldborder: Mutex<Worldborder>,
    /// The world's time, including counting ticks for weather, time cycles and statistics
    pub level_time: Mutex<LevelTime>,
    /// The type of dimension the world is in
    pub dimension_type: DimensionType,
    /// A map of active entities within the world, keyed by their unique UUID.
    pub current_living_entities: Arc<Mutex<HashMap<uuid::Uuid, Arc<LivingEntity>>>>,
    // TODO: entities
}

impl World {
    #[must_use]
    pub fn load(level: Level, dimension_type: DimensionType) -> Self {
        Self {
            level: Arc::new(level),
            current_players: Arc::new(Mutex::new(HashMap::new())),
            scoreboard: Mutex::new(Scoreboard::new()),
            worldborder: Mutex::new(Worldborder::new(0.0, 0.0, 29_999_984.0, 0, 0, 0)),
            level_time: Mutex::new(LevelTime::new()),
            dimension_type,
            current_living_entities: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn save(&self) {
        self.level.save().await;
    }

    /// Broadcasts a packet to all connected players within the world.
    ///
    /// Sends the specified packet to every player currently logged in to the world.
    ///
    /// **Note:** This function acquires a lock on the `current_players` map, ensuring thread safety.
    pub async fn broadcast_packet_all<P>(&self, packet: &P)
    where
        P: ClientPacket,
    {
        let current_players = self.current_players.lock().await;
        for player in current_players.values() {
            if let Some(client) = player.get_client() {
                client.send_packet(packet).await;
            }
        }
    }

    /// Broadcasts a packet to all connected players within the world, excluding the specified players.
    ///
    /// Sends the specified packet to every player currently logged in to the world, excluding the players listed in the `except` parameter.
    ///
    /// **Note:** This function acquires a lock on the `current_players` map, ensuring thread safety.
    pub async fn broadcast_packet_except<P>(&self, except: &[uuid::Uuid], packet: &P)
    where
        P: ClientPacket,
    {
        let current_players = self.current_players.lock().await;
        for (_, player) in current_players.iter().filter(|c| !except.contains(c.0)) {
            if let Some(client) = player.get_client() {
                client.send_packet(packet).await;
            }
        }
    }

    pub async fn play_sound(
        &self,
        sound_id: u16,
        category: SoundCategory,
        position: &Vector3<f64>,
    ) {
        let seed = thread_rng().gen::<f64>();
        self.broadcast_packet_all(&CSoundEffect::new(
            VarInt(i32::from(sound_id)),
            None,
            category,
            position.x,
            position.y,
            position.z,
            1.0,
            1.0,
            seed,
        ))
        .await;
    }

    pub async fn play_block_sound(&self, sound_id: u16, position: WorldPosition) {
        let new_vec = Vector3::new(
            f64::from(position.0.x) + 0.5,
            f64::from(position.0.y) + 0.5,
            f64::from(position.0.z) + 0.5,
        );
        self.play_sound(sound_id, SoundCategory::Blocks, &new_vec)
            .await;
    }

    pub async fn play_record(&self, record_id: i32, position: WorldPosition) {
        self.broadcast_packet_all(&CLevelEvent::new(1010, position, record_id, false))
            .await;
    }

    pub async fn stop_record(&self, position: WorldPosition) {
        self.broadcast_packet_all(&CLevelEvent::new(1011, position, 0, false))
            .await;
    }

    pub async fn tick(&self) {
        // world ticks
        let mut level_time = self.level_time.lock().await;
        level_time.tick_time();
        if level_time.world_age % 20 == 0 {
            level_time.send_time(self).await;
        }
        // player ticks
        let current_players = self.current_players.lock().await;
        for player in current_players.values() {
            player.tick().await;
        }
    }

    /// Gets the y position of the first non air block from the top down
    pub async fn get_top_block(&self, position: Vector2<i32>) -> i32 {
        for y in (-64..=319).rev() {
            let pos = WorldPosition(Vector3::new(position.x, y, position.z));
            let block = self.get_block_state(pos).await;
            if let Ok(block) = block {
                if block.air {
                    continue;
                }
            }
            return y;
        }
        319
    }

    #[expect(clippy::too_many_lines)]
    pub async fn spawn_player(
        &self,
        base_config: &BasicConfiguration,
        player: Arc<Player>,
        server: &Server,
    ) {
        let dimensions: Vec<Identifier> =
            server.dimensions.iter().map(DimensionType::name).collect();

        // This code follows the vanilla packet order
        let entity_id = player.entity_id();
        let player_gamemode = player.get_gamemode().expect("Player has no gamemode");
        let gamemode = player_gamemode.load();
        log::debug!(
            "spawning player {}, entity id {}",
            player.get_gameprofile().name,
            entity_id
        );

        // login packet for our new player
        let client = &player.get_client().expect("Player has no client");
        client
            .send_packet(&CLogin::new(
                entity_id,
                base_config.hardcore,
                &dimensions,
                base_config.max_players.into(),
                base_config.view_distance.get().into(), //  TODO: view distance
                base_config.simulation_distance.get().into(), // TODO: sim view dinstance
                false,
                true,
                false,
                (self.dimension_type as u8).into(),
                self.dimension_type.name(),
                0, // seed
                gamemode as u8,
                base_config.default_gamemode as i8,
                false,
                false,
                None,
                0.into(),
                0.into(),
                false,
            ))
            .await;
        // permissions, i. e. the commands a player may use
        player.send_permission_lvl_update().await;
        client_cmd_suggestions::send_c_commands_packet(&player, &server.command_dispatcher).await;
        // teleport
        let mut position = Vector3::new(10.0, 120.0, 10.0);
        let yaw = 10.0;
        let pitch = 10.0;

        let top = self
            .get_top_block(Vector2::new(position.x as i32, position.z as i32))
            .await;
        position.y = f64::from(top + 1);

        log::debug!(
            "Sending player teleport to {}",
            player.get_gameprofile().name
        );
        player.request_teleport(position, yaw, pitch).await;

        let living_entity = &player
            .get_living_entity()
            .expect("Player has no living entity");
        living_entity.last_pos.store(position);

        let gameprofile = &player.get_gameprofile();
        // first send info update to our new player, So he can see his Skin
        // also send his info to everyone else
        log::debug!(
            "Broadcasting player info for {}",
            player.get_gameprofile().name
        );
        self.broadcast_packet_all(&CPlayerInfoUpdate::new(
            0x01 | 0x08,
            &[pumpkin_protocol::client::play::Player {
                uuid: gameprofile.id,
                actions: vec![
                    PlayerAction::AddPlayer {
                        name: &gameprofile.name,
                        properties: &gameprofile.properties,
                    },
                    PlayerAction::UpdateListed(true),
                ],
            }],
        ))
        .await;
        player.update_client_information().await;

        // here we send all the infos of already joined players
        let mut entries = Vec::new();
        {
            let current_players = self.current_players.lock().await;
            for (_, playerr) in current_players
                .iter()
                .filter(|(c, _)| **c != player.get_gameprofile().id)
            {
                let gameprofile = &playerr.get_gameprofile();
                entries.push(pumpkin_protocol::client::play::Player {
                    uuid: gameprofile.id,
                    actions: vec![
                        PlayerAction::AddPlayer {
                            name: &gameprofile.name,
                            properties: &gameprofile.properties,
                        },
                        PlayerAction::UpdateListed(true),
                    ],
                });
            }
            log::debug!("Sending player info to {}", player.get_gameprofile().name);
            let client = &player.get_client().expect("Player has no client");
            client
                .send_packet(&CPlayerInfoUpdate::new(0x01 | 0x08, &entries))
                .await;
        };

        let gameprofile = &player.get_gameprofile();

        log::debug!(
            "Broadcasting player spawn for {}",
            player.get_gameprofile().name
        );
        // spawn player for every client
        self.broadcast_packet_except(
            &[player.get_gameprofile().id],
            // TODO: add velo
            &CSpawnEntity::new(
                entity_id.into(),
                gameprofile.id,
                (EntityType::Player as i32).into(),
                position.x,
                position.y,
                position.z,
                pitch,
                yaw,
                yaw,
                0.into(),
                0.0,
                0.0,
                0.0,
            ),
        )
        .await;
        // spawn players for our client
        let id = player.get_gameprofile().id;
        for (_, existing_player) in self
            .current_players
            .lock()
            .await
            .iter()
            .filter(|c| c.0 != &id)
        {
            let living_entity = &existing_player
                .get_living_entity()
                .expect("Player has no living entity");
            let entity = &living_entity.entity;
            let pos = entity.pos.load();
            let gameprofile = &existing_player.get_gameprofile();
            log::debug!(
                "Sending player entities to {}",
                player.get_gameprofile().name
            );
            let client = &player.get_client().expect("Player has no client");
            client
                .send_packet(&CSpawnEntity::new(
                    existing_player.entity_id().into(),
                    gameprofile.id,
                    (EntityType::Player as i32).into(),
                    pos.x,
                    pos.y,
                    pos.z,
                    entity.yaw.load(),
                    entity.pitch.load(),
                    entity.head_yaw.load(),
                    0.into(),
                    0.0,
                    0.0,
                    0.0,
                ))
                .await;
        }
        // entity meta data
        // set skin parts
        let client = &player.get_client().expect("Player has no client");
        if let Some(config) = client.config.lock().await.as_ref() {
            let packet = CSetEntityMetadata::new(
                entity_id.into(),
                Metadata::new(17, VarInt(0), config.skin_parts),
            );
            log::debug!("Broadcasting skin for {}", player.get_gameprofile().name);
            self.broadcast_packet_all(&packet).await;
        }

        // Start waiting for level chunks, Sets the "Loading Terrain" screen
        log::debug!(
            "Sending waiting chunks to {}",
            player.get_gameprofile().name
        );
        let client = &player.get_client().expect("Player has no client");
        client
            .send_packet(&CGameEvent::new(GameEvent::StartWaitingChunks, 0.0))
            .await;

        self.worldborder.lock().await.init_client(client).await;

        // Sends initial time
        player.send_time(self).await;

        // Spawn in initial chunks
        player_chunker::player_join(&player).await;

        // if let Some(bossbars) = self..lock().await.get_player_bars(&player.gameprofile.id) {
        //     for bossbar in bossbars {
        //         player.send_bossbar(bossbar).await;
        //     }
        // }
    }

    pub async fn respawn_player(&self, player: &Arc<Player>, alive: bool) {
        let living_entity = &player
            .get_living_entity()
            .expect("Player has no living entity");
        let last_pos = living_entity.last_pos.load();
        let death_dimension = player.world().dimension_type.name();
        let death_location = WorldPosition(Vector3::new(
            last_pos.x.round() as i32,
            last_pos.y.round() as i32,
            last_pos.z.round() as i32,
        ));

        let data_kept = u8::from(alive);

        // TODO: switch world in player entity to new world

        let client = &player.get_client().expect("Player has no client");
        let gamemode = &player.get_gamemode().expect("Player has no gamemode");
        client
            .send_packet(&CRespawn::new(
                (self.dimension_type as u8).into(),
                self.dimension_type.name(),
                0, // seed
                gamemode.load() as u8,
                gamemode.load() as i8,
                false,
                false,
                Some((death_dimension, death_location)),
                0.into(),
                0.into(),
                data_kept,
            ))
            .await;

        log::debug!(
            "Sending player abilities to {}",
            player.get_gameprofile().name
        );
        player.send_abilities_update().await;

        player.send_permission_lvl_update().await;

        // teleport
        let mut position = Vector3::new(10.0, 120.0, 10.0);
        let yaw = 10.0;
        let pitch = 10.0;

        let top = self
            .get_top_block(Vector2::new(position.x as i32, position.z as i32))
            .await;
        position.y = f64::from(top + 1);

        log::debug!(
            "Sending player teleport to {}",
            player.get_gameprofile().name
        );
        player.request_teleport(position, yaw, pitch).await;

        let living_entity = &player
            .get_living_entity()
            .expect("Player has no living entity");
        living_entity.last_pos.store(position);

        // TODO: difficulty, exp bar, status effect

        self.worldborder.lock().await.init_client(client).await;

        // TODO: world spawn (compass stuff)

        client
            .send_packet(&CGameEvent::new(GameEvent::StartWaitingChunks, 0.0))
            .await;

        let living_entity = &player
            .get_living_entity()
            .expect("Player has no living entity");
        let entity = &living_entity.entity;
        let entity_id = entity.entity_id;

        let skin_parts;
        {
            let player_config = &player.get_config().expect("Player has no config");
            skin_parts = player_config.lock().await.skin_parts;
        }

        let entity_metadata_packet =
            CSetEntityMetadata::new(entity_id.into(), Metadata::new(17, VarInt(0), &skin_parts));

        self.broadcast_packet_except(
            &[player.get_gameprofile().id],
            // TODO: add velo
            &CSpawnEntity::new(
                entity.entity_id.into(),
                player.get_gameprofile().id,
                (EntityType::Player as i32).into(),
                position.x,
                position.y,
                position.z,
                pitch,
                yaw,
                yaw,
                0.into(),
                0.0,
                0.0,
                0.0,
            ),
        )
        .await;

        log::debug!("Sending player entities to {}", player.get_gameprofile().name);

        player_chunker::player_join(player).await;

        log::debug!("Sending player metadata to {}", player.get_gameprofile().name);
        self.broadcast_packet_all(&entity_metadata_packet).await;
        // update commands

        log::debug!("Setting health for {}", player.get_gameprofile().name);

        player.set_health(20.0, 20, 20.0).await;

        log::debug!("Respawned player {}", player.get_gameprofile().name);
    }

    /// IMPORTANT: Chunks have to be non-empty
    fn spawn_world_chunks(
        &self,
        player: Arc<Player>,
        chunks: Vec<Vector2<i32>>,
        center_chunk: Vector2<i32>,
    ) {
        let client = &player.get_client().expect("Player has no client");
        if client.closed.load(std::sync::atomic::Ordering::Relaxed) {
            log::info!("The connection has closed before world chunks were spawned",);
            return;
        }
        #[cfg(debug_assertions)]
        let inst = std::time::Instant::now();

        // Sort such that the first chunks are closest to the center
        let mut chunks = chunks;
        chunks.sort_unstable_by_key(|pos| {
            let rel_x = pos.x - center_chunk.x;
            let rel_z = pos.z - center_chunk.z;
            rel_x * rel_x + rel_z * rel_z
        });

        let mut receiver = self.receive_chunks(chunks);
        let level = self.level.clone();

        tokio::spawn(async move {
            while let Some(chunk_data) = receiver.recv().await {
                let chunk_data = chunk_data.read().await;
                let packet = CChunkData(&chunk_data);
                #[cfg(debug_assertions)]
                if chunk_data.position == (0, 0).into() {
                    let mut test = bytes::BytesMut::new();
                    packet.write(&mut test);
                    let len = test.len();
                    log::debug!(
                        "Chunk packet size: {}B {}KB {}MB",
                        len,
                        len / 1024,
                        len / (1024 * 1024)
                    );
                }

                if !level.is_chunk_watched(&chunk_data.position) {
                    log::trace!(
                        "Received chunk {:?}, but it is no longer watched... cleaning",
                        &chunk_data.position
                    );
                    level.clean_chunk(&chunk_data.position);
                    continue;
                }

                let client = &player.get_client().expect("Player has no client");
                if !client.closed.load(std::sync::atomic::Ordering::Relaxed) {
                    client.send_packet(&packet).await;
                }
            }

            #[cfg(debug_assertions)]
            log::debug!("chunks sent after {}ms ", inst.elapsed().as_millis(),);
        });
    }

    /// Gets a Player by entity id
    pub async fn get_player_by_entityid(&self, id: EntityId) -> Option<Arc<Player>> {
        for player in self.current_players.lock().await.values() {
            if player.entity_id() == id {
                return Some(player.clone());
            }
        }
        None
    }

    /// Gets a Living Entity by entity id
    pub async fn get_living_entity_by_entityid(&self, id: EntityId) -> Option<Arc<LivingEntity>> {
        for living_entity in self.current_living_entities.lock().await.values() {
            if living_entity.entity_id() == id {
                return Some(living_entity.clone());
            }
        }
        None
    }

    /// Gets a Player by username
    pub async fn get_player_by_name(&self, name: &str) -> Option<Arc<Player>> {
        for player in self.current_players.lock().await.values() {
            if player.get_gameprofile().name == name {
                return Some(player.clone());
            }
        }
        None
    }

    /// Retrieves a player by their unique UUID.
    ///
    /// This function searches the world's active player list for a player with the specified UUID.
    /// If found, it returns an `Arc<Player>` reference to the player. Otherwise, it returns `None`.
    ///
    /// # Arguments
    ///
    /// * `id`: The UUID of the player to retrieve.
    ///
    /// # Returns
    ///
    /// An `Option<Arc<Player>>` containing the player if found, or `None` if not.
    pub async fn get_player_by_uuid(&self, id: uuid::Uuid) -> Option<Arc<Player>> {
        return self.current_players.lock().await.get(&id).cloned();
    }

    /// Gets a list of players who's location equals the given position in the world.
    ///
    /// It iterates through the players in the world and checks their location. If the player's location matches the
    /// given position it will add this to a Vec which it later returns. If no
    /// player was found in that position it will just return an empty Vec.
    ///
    /// # Arguments
    ///
    /// * `position`: The position the function will check.
    pub async fn get_players_by_pos(
        &self,
        position: WorldPosition,
    ) -> HashMap<uuid::Uuid, Arc<Player>> {
        self.current_players
            .lock()
            .await
            .iter()
            .filter_map(|(uuid, player)| {
                let living_entity = &player
                    .get_living_entity()
                    .expect("Player has no living entity");
                let player_block_pos = living_entity.entity.block_pos.load().0;
                (position.0.x == player_block_pos.x
                    && position.0.y == player_block_pos.y
                    && position.0.z == player_block_pos.z)
                    .then(|| (*uuid, Arc::clone(player)))
            })
            .collect::<HashMap<uuid::Uuid, Arc<Player>>>()
    }

    /// Gets the nearby players around a given world position
    /// It "creates" a sphere and checks if whether players are inside
    /// and returns a hashmap where the uuid is the key and the player
    /// object the value.
    ///
    /// # Arguments
    /// * `pos`: The middlepoint of the sphere
    /// * `radius`: The radius of the sphere. The higher the radius
    ///             the more area will be checked, in every direction.
    pub async fn get_nearby_players(
        &self,
        pos: Vector3<f64>,
        radius: u16,
    ) -> HashMap<uuid::Uuid, Arc<Player>> {
        let radius_squared = (f64::from(radius)).powi(2);

        let mut found_players = HashMap::new();
        for player in self.current_players.lock().await.iter() {
            let living_entity = &player
                .1
                .get_living_entity()
                .expect("Player has no living entity");
            let player_pos = living_entity.entity.pos.load();

            let diff = Vector3::new(
                player_pos.x - pos.x,
                player_pos.y - pos.y,
                player_pos.z - pos.z,
            );

            let distance_squared = diff.x.powi(2) + diff.y.powi(2) + diff.z.powi(2);
            if distance_squared <= radius_squared {
                found_players.insert(*player.0, player.1.clone());
            }
        }

        found_players
    }

    /// Adds a player to the world and broadcasts a join message if enabled.
    ///
    /// This function takes a player's UUID and an `Arc<Player>` reference.
    /// It inserts the player into the world's `current_players` map using the UUID as the key.
    /// Additionally, it may broadcasts a join message to all connected players in the world.
    ///
    /// # Arguments
    ///
    /// * `uuid`: The unique UUID of the player to add.
    /// * `player`: An `Arc<Player>` reference to the player object.
    pub async fn add_player(&self, uuid: uuid::Uuid, player: Arc<Player>) {
        let mut current_players = self.current_players.lock().await;
        current_players.insert(uuid, player.clone());

        // Handle join message
        // TODO: Config
        let msg_txt = format!(
            "{} joined the game.",
            player.get_gameprofile().name.as_str()
        );
        let msg_comp = TextComponent::text(msg_txt).color_named(NamedColor::Yellow);
        for player in current_players.values() {
            player.send_system_message(&msg_comp).await;
        }
        log::info!("{}", msg_comp.to_pretty_console());
    }

    /// Removes a player from the world and broadcasts a disconnect message if enabled.
    ///
    /// This function removes a player from the world based on their `Player` reference.
    /// It performs the following actions:
    ///
    /// 1. Removes the player from the `current_players` map using their UUID.
    /// 2. Broadcasts a `CRemovePlayerInfo` packet to all connected players to inform them about the player leaving.
    /// 3. Removes the player's entity from the world using its entity ID.
    /// 4. Optionally sends a disconnect message to all other players notifying them about the player leaving.
    ///
    /// # Arguments
    ///
    /// * `player`: A reference to the `Player` object to be removed.
    ///
    /// # Notes
    ///
    /// - This function assumes `broadcast_packet_expect` and `remove_entity` are defined elsewhere.
    /// - The disconnect message sending is currently optional. Consider making it a configurable option.
    pub async fn remove_player(&self, player: &Player) {
        self.current_players
            .lock()
            .await
            .remove(&player.get_gameprofile().id)
            .unwrap();
        let uuid = player.get_gameprofile().id;
        self.broadcast_packet_except(
            &[player.get_gameprofile().id],
            &CRemovePlayerInfo::new(1.into(), &[uuid]),
        )
        .await;
        let living_entity = &player
            .get_living_entity()
            .expect("Player has no living entity");
        self.remove_entity(&living_entity.entity).await;

        // Send disconnect message / quit message to players in the same world
        // TODO: Config
        let disconn_msg_txt = format!("{} left the game.", player.get_gameprofile().name.as_str());
        let disconn_msg_cmp = TextComponent::text(disconn_msg_txt).color_named(NamedColor::Yellow);
        for player in self.current_players.lock().await.values() {
            player.send_system_message(&disconn_msg_cmp).await;
        }
        log::info!("{}", disconn_msg_cmp.to_pretty_console());
    }

    /// Adds a living entity to the world.
    ///
    /// This function takes a living entity's UUID and an `Arc<LivingEntity>` reference.
    /// It inserts the living entity into the world's `current_living_entities` map using the UUID as the key.
    ///
    /// # Arguments
    ///
    /// * `uuid`: The unique UUID of the living entity to add.
    /// * `living_entity`: A `Arc<LivingEntity>` reference to the living entity object.
    pub async fn add_living_entity(&self, uuid: uuid::Uuid, living_entity: Arc<LivingEntity>) {
        let mut current_living_entities = self.current_living_entities.lock().await;
        current_living_entities.insert(uuid, living_entity);
    }

    pub async fn remove_living_entity(living_entity: Arc<LivingEntity>, world: Arc<Self>) {
        let mut current_living_entities = world.current_living_entities.lock().await.clone();
        // TODO: does this work with collisions?
        living_entity.entity.set_pose(EntityPose::Dying).await;
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
            world.remove_entity(&living_entity.entity).await;
            current_living_entities.remove(&living_entity.entity.entity_uuid);
        });
    }

    pub async fn remove_entity(&self, entity: &Entity) {
        self.broadcast_packet_all(&CRemoveEntities::new(&[entity.entity_id.into()]))
            .await;
    }

    /// Sets a block
    pub async fn set_block_state(&self, position: WorldPosition, block_state_id: u16) -> u16 {
        let (chunk_coordinate, relative_coordinates) = position.chunk_and_chunk_relative_position();

        // Since we divide by 16 remnant can never exceed u8
        let relative = ChunkRelativeBlockCoordinates::from(relative_coordinates);

        let chunk = self.receive_chunk(chunk_coordinate).await;
        let replaced_block_state_id = chunk
            .write()
            .await
            .blocks
            .set_block(relative, block_state_id);

        self.broadcast_packet_all(&CBlockUpdate::new(
            &position,
            i32::from(block_state_id).into(),
        ))
        .await;

        replaced_block_state_id
    }

    // Stream the chunks (don't collect them and then do stuff with them)
    /// Important: must be called from an async function (or changed to accept a tokio runtime
    /// handle)
    pub fn receive_chunks(&self, chunks: Vec<Vector2<i32>>) -> Receiver<Arc<RwLock<ChunkData>>> {
        let (sender, receive) = mpsc::channel(chunks.len());
        // Put this in another thread so we aren't blocking on it
        let level = self.level.clone();
        let rt = Handle::current();
        rayon::spawn(move || {
            level.fetch_chunks(&chunks, sender, &rt);
        });
        receive
    }

    pub async fn receive_chunk(&self, chunk_pos: Vector2<i32>) -> Arc<RwLock<ChunkData>> {
        let mut receiver = self.receive_chunks(vec![chunk_pos]);
        let chunk = receiver
            .recv()
            .await
            .expect("Channel closed for unknown reason");

        if !self.level.is_chunk_watched(&chunk_pos) {
            log::trace!(
                "Received chunk {:?}, but it is not watched... cleaning",
                chunk_pos
            );
            self.level.clean_chunk(&chunk_pos);
        }

        chunk
    }

    pub async fn break_block(&self, position: WorldPosition, cause: Option<&Player>) {
        let broken_block_state_id = self.set_block_state(position, 0).await;

        let particles_packet =
            CWorldEvent::new(2001, &position, broken_block_state_id.into(), false);

        match cause {
            Some(player) => {
                self.broadcast_packet_except(&[player.get_gameprofile().id], &particles_packet)
                    .await;
            }
            None => self.broadcast_packet_all(&particles_packet).await,
        }
    }

    pub async fn get_block_state_id(&self, position: WorldPosition) -> Result<u16, GetBlockError> {
        let (chunk, relative) = position.chunk_and_chunk_relative_position();
        let relative = ChunkRelativeBlockCoordinates::from(relative);
        let chunk = self.receive_chunk(chunk).await;
        let chunk: tokio::sync::RwLockReadGuard<ChunkData> = chunk.read().await;

        let Some(id) = chunk.blocks.get_block(relative) else {
            return Err(GetBlockError::BlockOutOfWorldBounds);
        };

        Ok(id)
    }

    /// Gets the Block from the Block Registry, Returns None if the Block has not been found
    pub async fn get_block(
        &self,
        position: WorldPosition,
    ) -> Result<&pumpkin_world::block::block_registry::Block, GetBlockError> {
        let id = self.get_block_state_id(position).await?;
        get_block_by_state_id(id).ok_or(GetBlockError::InvalidBlockId)
    }

    /// Gets the Block state from the Block Registry, Returns None if the Block state has not been found
    pub async fn get_block_state(
        &self,
        position: WorldPosition,
    ) -> Result<&pumpkin_world::block::block_registry::State, GetBlockError> {
        let id = self.get_block_state_id(position).await?;
        get_state_by_state_id(id).ok_or(GetBlockError::InvalidBlockId)
    }

    /// Gets the Block + Block state from the Block Registry, Returns None if the Block state has not been found
    pub async fn get_block_and_block_state(
        &self,
        position: WorldPosition,
    ) -> Result<
        (
            &pumpkin_world::block::block_registry::Block,
            &pumpkin_world::block::block_registry::State,
        ),
        GetBlockError,
    > {
        let id = self.get_block_state_id(position).await?;
        get_block_and_state_by_state_id(id).ok_or(GetBlockError::InvalidBlockId)
    }
}
