//!HOOK MAIN
//!BIND HOOKED
//!DESC Per-frame noise: an incompressible source that fills any PyroWave frame budget
// mpv user shader for scripts/pyrowave_high_bitrate/run.py. Every pixel of
// every frame is new, so the encoder always meets its full per-frame budget
// and the transport carries the configured bitrate. A worst case, not a game.
uint hash(uint x) {
	x ^= x >> 16;
	x *= 0x7feb352du;
	x ^= x >> 15;
	x *= 0x846ca68bu;
	x ^= x >> 16;
	return x;
}

vec4 hook() {
	uvec2 p = uvec2(HOOKED_pos * HOOKED_size);
	uint h = hash(p.x + p.y * 7919u + uint(frame) * 3874973u);
	return vec4(float(h & 255u), float((h >> 8) & 255u), float((h >> 16) & 255u), 255.0) / 255.0;
}
