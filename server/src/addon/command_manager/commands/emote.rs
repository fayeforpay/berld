use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::str::SplitWhitespace;

use png::{ColorType, Decoder, OutputInfo, Transformations};
use rand::Rng;

use protocol::nalgebra::Point3;
use protocol::packet::WorldUpdate;
use protocol::packet::world_update::{particle, Particle};
use protocol::rgb::RGBA;
use protocol::utils::constants::SIZE_BLOCK;

use crate::addon::command_manager::{Command, CommandResult};
use crate::addon::command_manager::commands::Emote;
use crate::addon::command_manager::utils::INGAME_ONLY;
use crate::addon::events::utils::{find_players_by_distance, RENDER_DISTANCE_CREATURE};
use crate::server::player::Player;
use crate::server::Server;

const IMAGES_DIR: &str = "images/emotes";
const CHUNK_SIZE: usize = 512;
const PARTICLE_SIZE: f32 = 0.025;
const PARTICLE_CAP: usize = 5000;

impl Command for Emote {
	const LITERAL: &'static str = "emote";
	const ADMIN_ONLY: bool = false;

	async fn execute<'fut>(&'fut self, server: &'fut Server, caller: Option<&'fut Player>, params: &'fut mut SplitWhitespace<'fut>) -> CommandResult {
		let caller = caller.ok_or(INGAME_ONLY)?;

		let (name, usage_hint) = match params.next() {
			Some(name) => (name.to_string(), None),
			None => {
				let name = random_emote().ok_or("emote folder is empty")?;
				let hint = format!("/emote {name}");
				(name, Some(hint))
			}
		};

		let path = resolve_image_path(&name)?;

		let (info, buf) = tokio::task::spawn_blocking(move || decode_png(&path))
			.await
			.map_err(|_| "image decode panicked")??;

		let character = caller.character.read().await;
		let position = character.position;
		let head_z = position.z + (character.appearance.creature_size.height * SIZE_BLOCK as f32) as i64;
		let yaw = character.rotation.yaw;
		drop(character);

		let base_x = position.x;
		let base_y = position.y;
		let spacing = PARTICLE_SIZE * SIZE_BLOCK as f32;
		let base_z = head_z + spacing.round() as i64;

		let yaw_rad = (yaw as f64).to_radians();
		let cos_yaw = yaw_rad.cos();
		let sin_yaw = yaw_rad.sin();

		let (width, height) = (info.width, info.height);

		let mut particles: Vec<Particle> = (0..height)
			.flat_map(|row| (0..width).map(move |column| (column, row)))
			.filter_map(|(column, row)| {
				let color = read_pixel(&buf, &info, column, row)?;
				let (dx, dz) = particle_offset(column, row, width, height, spacing);
				let rotated_x = (dx as f64 * cos_yaw).round() as i64;
				let rotated_y = (dx as f64 * sin_yaw).round() as i64;

				Some(new_particle(
					Point3::new(base_x + rotated_x, base_y + rotated_y, base_z + dz),
					PARTICLE_SIZE,
					1,
					color
				))
			})
			.collect();

		if particles.len() < PARTICLE_CAP {
			particles.push(new_particle(
				Point3::new(base_x, base_y, base_z),
				0.0,
				(PARTICLE_CAP - particles.len()) as i32,
				RGBA::new(0.0, 0.0, 0.0, 0.0),
			));
		}

		let recipients = find_players_by_distance(server, position, RENDER_DISTANCE_CREATURE).await;

		while !particles.is_empty() {
			let chunk: Vec<Particle> = particles.drain(..particles.len().min(CHUNK_SIZE)).collect();
			let packet = WorldUpdate::from(chunk);

			for player in &recipients {
				player.send_ignoring(&packet).await
			}
		}

		Ok(usage_hint)
	}
}

fn random_emote() -> Option<String> {
	let names: Vec<String> = std::fs::read_dir(IMAGES_DIR)
		.ok()?
		.flatten()
		.map(|entry| entry.path())
		.filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("png"))
		.filter_map(|path| path.file_stem()?.to_str().map(String::from))
		.collect();

	if names.is_empty() { return None }

	Some(names[rand::rng().random_range(0..names.len())].clone())
}

fn new_particle(position: Point3<i64>, size: f32, count: i32, color: RGBA<f32>) -> Particle {
	Particle {
		position,
		velocity: [0.0, 0.0, 0.0].into(),
		color,
		size,
		count,
		kind: particle::Kind::NoSpreadNoRotation,
		spread: 0.0
	}
}

fn resolve_image_path(name: &str) -> Result<PathBuf, &'static str> {
	let filename = Path::new(name)
		.file_name()
		.and_then(|file| file.to_str())
		.ok_or("invalid emote name")?;

	Ok(Path::new(IMAGES_DIR).join(format!("{filename}.png")))
}

fn decode_png(path: &Path) -> Result<(OutputInfo, Vec<u8>), &'static str> {
	let bytes = std::fs::read(path).map_err(|_| "failed to read emote file")?;

	let mut decoder = Decoder::new(Cursor::new(bytes));
	decoder.set_transformations(Transformations::normalize_to_color8());

	let mut reader = decoder.read_info().map_err(|_| "invalid png")?;
	let mut buf = vec![0_u8; reader.output_buffer_size().ok_or("image too large")?];
	let info = reader.next_frame(buf.as_mut_slice()).map_err(|_| "failed to decode image")?;
	buf.truncate(info.buffer_size());

	Ok(downscale_optional(info, buf))
}

fn channels_of(color_type: ColorType) -> usize {
	match color_type {
		ColorType::Grayscale      => 1,
		ColorType::GrayscaleAlpha => 2,
		ColorType::Rgb            => 3,
		ColorType::Rgba           => 4,
		ColorType::Indexed        => unreachable!("expanded away")
	}
}

fn downscale_optional(info: OutputInfo, buf: Vec<u8>) -> (OutputInfo, Vec<u8>) {
	let area = info.width as u64 * info.height as u64;
	if area <= PARTICLE_CAP as u64 { return (info, buf) }

	let (new_width, new_height) = fit_under_cap(info.width, info.height, PARTICLE_CAP as u64);
	let channels = channels_of(info.color_type);
	let resampled = resample_box(&buf, &info, channels, new_width, new_height);

	let new_info = OutputInfo {
		width: new_width,
		height: new_height,
		line_size: new_width as usize * channels,
		..info
	};

	(new_info, resampled)
}

fn fit_under_cap(width: u32, height: u32, cap: u64) -> (u32, u32) {
	let area = width as u64 * height as u64;
	let scale = (cap as f64 / area as f64).sqrt();

	let mut new_width = ((width as f64 * scale).floor() as u32).max(1);
	let mut new_height = ((height as f64 * scale).floor() as u32).max(1);

	while new_width as u64 * new_height as u64 > cap {
		if new_width >= new_height { new_width -= 1 } else { new_height -= 1 }
	}

	(new_width, new_height)
}

fn resample_box(buf: &[u8], info: &OutputInfo, channels: usize, new_width: u32, new_height: u32) -> Vec<u8> {
	let mut out = vec![0_u8; new_width as usize * new_height as usize * channels];

	for oy in 0..new_height {
		let sy_start = oy * info.height / new_height;
		let sy_end = ((oy + 1) * info.height / new_height).max(sy_start + 1);

		for ox in 0..new_width {
			let sx_start = ox * info.width / new_width;
			let sx_end = ((ox + 1) * info.width / new_width).max(sx_start + 1);

			let count = (sx_end - sx_start) * (sy_end - sy_start);
			let (mut r, mut g, mut b, mut a_sum) = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);

			for sy in sy_start..sy_end {
				for sx in sx_start..sx_end {
					let (px_r, px_g, px_b, px_a) = raw_pixel(buf, info.color_type, info.line_size, channels, sx, sy);
					let a = px_a as f64 / 255.0;

					r += px_r as f64 / 255.0 * a;
					g += px_g as f64 / 255.0 * a;
					b += px_b as f64 / 255.0 * a;
					a_sum += a;
				}
			}

			let (out_r, out_g, out_b) = if a_sum > 0.0 {
				(r / a_sum, g / a_sum, b / a_sum)
			} else {
				(0.0, 0.0, 0.0)
			};
			let out_a = a_sum / count as f64;

			let dst = (oy as usize * new_width as usize + ox as usize) * channels;
			write_pixel(&mut out[dst..dst + channels], info.color_type, out_r, out_g, out_b, out_a);
		}
	}

	out
}

fn raw_pixel(buf: &[u8], color_type: ColorType, line_size: usize, channels: usize, column: u32, row: u32) -> (u8, u8, u8, u8) {
	let offset = row as usize * line_size + column as usize * channels;
	let px = &buf[offset..offset + channels];

	match color_type {
		ColorType::Grayscale      => (px[0], px[0], px[0], 255),
		ColorType::GrayscaleAlpha => (px[0], px[0], px[0], px[1]),
		ColorType::Rgb            => (px[0], px[1], px[2], 255),
		ColorType::Rgba           => (px[0], px[1], px[2], px[3]),
		ColorType::Indexed        => unreachable!("expanded away")
	}
}

fn write_pixel(dst: &mut [u8], color_type: ColorType, r: f64, g: f64, b: f64, a: f64) {
	let to_byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;

	match color_type {
		ColorType::Grayscale      => dst[0] = to_byte(r),
		ColorType::GrayscaleAlpha => { dst[0] = to_byte(r); dst[1] = to_byte(a); }
		ColorType::Rgb            => { dst[0] = to_byte(r); dst[1] = to_byte(g); dst[2] = to_byte(b); }
		ColorType::Rgba           => { dst[0] = to_byte(r); dst[1] = to_byte(g); dst[2] = to_byte(b); dst[3] = to_byte(a); }
		ColorType::Indexed        => unreachable!("expanded away")
	}
}

fn read_pixel(buf: &[u8], info: &OutputInfo, column: u32, row: u32) -> Option<RGBA<f32>> {
	let channels = channels_of(info.color_type);
	let (r, g, b, a) = raw_pixel(buf, info.color_type, info.line_size, channels, column, row);

	if a == 0 { return None }

	Some(RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0))
}

fn particle_offset(column: u32, row: u32, width: u32, height: u32, spacing: f32) -> (i64, i64) {
	let x = ((column as f32 - width as f32 / 2.0) * spacing).round() as i64;
	let z = ((height as f32 - 1.0 - row as f32) * spacing).round() as i64;
	(x, z)
}