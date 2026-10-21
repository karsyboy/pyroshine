import { useEffect, useState } from "react";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Chip from "@mui/material/Chip";
import LinearProgress from "@mui/material/LinearProgress";
import Stack from "@mui/material/Stack";
import Table from "@mui/material/Table";
import TableBody from "@mui/material/TableBody";
import TableCell from "@mui/material/TableCell";
import TableHead from "@mui/material/TableHead";
import TableRow from "@mui/material/TableRow";
import Typography from "@mui/material/Typography";
import StopCircleOutlined from "@mui/icons-material/StopCircleOutlined";
import AddLink from "@mui/icons-material/AddLink";
import CheckCircleOutlined from "@mui/icons-material/CheckCircleOutlined";
import InfoOutlined from "@mui/icons-material/InfoOutlined";
import WarningAmber from "@mui/icons-material/WarningAmber";
import { api, errorMessage } from "../api/client";
import type { DesktopStatus, AudioDetails, CapturePathStats, CaptureStats, Finding, GpuEngineStats, GpuStats, SessionDetails, SessionPhase, SteamGameState, StreamStats } from "../api/types";
import { ConfirmDialog, Facts, Hint, PageHeader, StatusDot, phaseLabels } from "../components/common";
import { Sparkline } from "../components/Sparkline";
import { useDaemon, useNow } from "../state/daemon";
import { usePreferences } from "../state/preferences";
import { bitrate, bytes, channels, duration, micros, percent } from "../util/format";

const headlines: Record<SessionPhase, string> = {
  idle: "Pyroshine is ready for connections.",
  starting: "Starting the session…",
  streaming: "Streaming",
  client_disconnected: "Session running",
  reconnecting: "Client reconnecting…",
  stopping: "Ending the session…",
  error: "The session could not be stopped cleanly",
};

function formatChips(session: SessionDetails, stats: StreamStats | null): string[] {
  const video = session.video;
  if (!video) return [];
  const chips = [video.codec_label, video.dynamic_range === "SDR" ? "SDR" : "HDR", video.chroma, `${video.bit_depth}-bit`];
  if (video.encrypted) chips.push("Encrypted");
  if (stats?.epoch === session.epoch && stats.capture?.pacing === "vrr") chips.push("VRR capture");
  return chips;
}

// Steam's own progress, read from its logs (moonshine-core session/steam_game.rs).
const steamGameStates: Record<SteamGameState, string> = {
  launching: "Launching",
  updating: "Updating",
  running: "Running",
  exited: "Exited (Steam still open)",
};

const pacingLabels: Record<CaptureStats["pacing"], string> = {
  vrr: "VRR capture",
  fixed: "Fixed refresh",
  mixed: "Changing",
};

const pathLabels: Record<CapturePathStats["current"], string> = {
  direct: "Direct export",
  direct_late: "Direct export + overlay",
  composited: "Composited",
  mixed: "Mixed",
};

/// What the capture path means for the operator, with the main reason for composition.
function pathDetail(path: CapturePathStats): string {
  const reason = path.composition_reasons[0];
  switch (path.current) {
    case "direct":
      return "The game's own buffer goes straight to the encoder.";
    case "direct_late":
      return "Direct export; the encoder draws the cursor or a notification.";
    case "composited":
      return reason ? `${reason.label}.` : "The compositor renders every frame first.";
    case "mixed": {
      const total = path.direct_frames + path.direct_late_frames + path.composited_frames;
      const share = total ? Math.round((path.composited_frames * 100) / total) : 0;
      return reason ? `${share} % composited: ${reason.label.charAt(0).toLowerCase()}${reason.label.slice(1)}.` : `${share} % composited.`;
    }
  }
}

function FindingRow({ finding }: { finding: Finding }) {
  const warning = finding.severity === "warning";
  return (
    <Stack direction="row" sx={{ gap: 1.5, alignItems: "flex-start" }}>
      {warning ? <WarningAmber color="warning" fontSize="small" sx={{ mt: 0.25 }} /> : <InfoOutlined color="info" fontSize="small" sx={{ mt: 0.25 }} />}
      <Box sx={{ minWidth: 0 }}>
        <Typography variant="body2" sx={{ fontWeight: 600 }}>
          {finding.title}
        </Typography>
        <Typography variant="body2">{finding.detail}</Typography>
        {finding.hint && <Hint text={finding.hint} />}
      </Box>
    </Stack>
  );
}

/// The daemon's reading of the latest window: what limits the stream on the host.
/// Informational findings can be hidden under Settings → Diagnostics.
function Diagnosis({ findings: all }: { findings: Finding[] }) {
  const { infoFindings } = usePreferences();
  const findings = infoFindings ? all : all.filter((finding) => finding.severity === "warning");
  const hidden = all.length - findings.length;
  return (
    <Card sx={{ mb: 3 }} aria-label="Diagnosis">
      <CardContent>
        <Typography variant="h6" sx={{ mb: 1.5 }}>
          Diagnosis
        </Typography>
        {findings.length === 0 ? (
          <Stack direction="row" sx={{ gap: 1.5, alignItems: "center" }}>
            <CheckCircleOutlined color="success" fontSize="small" />
            <Typography variant="body2" color="text.secondary">
              {hidden > 0
                ? "No warnings."
                : "Nothing on the host limits the stream right now. Problems on the client or network after the host are not visible here."}
            </Typography>
          </Stack>
        ) : (
          <Stack sx={{ gap: 2 }}>
            {findings.map((finding) => (
              <FindingRow key={finding.id} finding={finding} />
            ))}
          </Stack>
        )}
        {hidden > 0 && (
          <Typography variant="caption" color="text.secondary" component="p" sx={{ mt: 1.5 }}>
            {hidden} informational {hidden === 1 ? "finding is" : "findings are"} hidden; show them under Settings → Diagnostics.
          </Typography>
        )}
      </CardContent>
    </Card>
  );
}

function Hero({ onEnd, navigate }: { onEnd: () => void; navigate: (to: string) => void }) {
  const daemon = useDaemon();
  const now = useNow();
  const phase = daemon.session?.phase ?? "idle";
  const session = daemon.session?.session;
  const canEnd = phase === "streaming" || phase === "starting" || phase === "reconnecting" || phase === "client_disconnected";
  const mode = session?.video
    ? `${session.video.width} × ${session.video.height} @ ${session.video.fps} Hz`
    : session
      ? `${session.requested.width} × ${session.requested.height} @ ${session.requested.refresh_rate} Hz`
      : null;

  return (
    <Card
      sx={{
        mb: 3,
        borderColor: phase === "streaming" ? "success.main" : phase === "client_disconnected" ? "warning.main" : "divider",
        borderWidth: phase === "streaming" || phase === "client_disconnected" ? 2 : 1,
      }}
    >
      {(phase === "starting" || phase === "reconnecting" || phase === "stopping") && <LinearProgress />}
      <CardContent sx={{ p: { xs: 3, md: 4 } }}>
        <Stack direction="row" sx={{ alignItems: "center", gap: 1.25, mb: 2 }}>
          <StatusDot phase={phase} />
          <Typography variant="overline" sx={{ color: "text.secondary", lineHeight: 1 }}>
            {phaseLabels[phase]}
          </Typography>
        </Stack>

        {!session ? (
          <Stack sx={{ gap: 2, alignItems: "flex-start" }}>
            <Typography variant="h4">{headlines[phase]}</Typography>
            <Typography color="text.secondary">
              {daemon.server
                ? `Moonlight clients on your network can find “${daemon.server.name}” and start an application.`
                : null}
            </Typography>
            <Button variant="outlined" startIcon={<AddLink />} onClick={() => navigate("clients")}>
              Pair a client
            </Button>
          </Stack>
        ) : (
          <Stack direction={{ xs: "column", md: "row" }} sx={{ gap: 3, justifyContent: "space-between" }}>
            <Box sx={{ minWidth: 0 }}>
              <Typography variant="h4" sx={{ mb: 0.5, overflowWrap: "anywhere" }}>
                {session.foreground_application?.title ?? session.application.title}
              </Typography>
              {mode && (
                <Typography variant="h6" color="text.secondary" sx={{ fontWeight: 400 }}>
                  {mode}
                </Typography>
              )}
              <Stack direction="row" sx={{ gap: 1, mt: 1.5, flexWrap: "wrap" }}>
                {formatChips(session, daemon.stats).map((chip) => (
                  <Chip key={chip} label={chip} size="small" variant="outlined" />
                ))}
              </Stack>
              {phase === "client_disconnected" && (
                <Alert severity="warning" variant="outlined" sx={{ mt: 2 }}>
                  Client disconnected. {session.capture?.mode === "desktop" ? "Displays and audio restore after the configured reconnect grace period." : `${session.application.title} keeps running until you end the session or resume from Moonlight.`}
                </Alert>
              )}
              {phase === "reconnecting" && (
                <Alert severity="info" variant="outlined" sx={{ mt: 2 }}>
                  The client is resuming the session.
                </Alert>
              )}
            </Box>
            <Stack sx={{ gap: 2, minWidth: 240, alignItems: { md: "flex-end" } }}>
              <Facts
                rows={[
                  ["Client", session.client_address],
                  ["Target bitrate", session.video ? bitrate(session.video.bitrate_bps) : "—"],
                  ["Live bitrate", phase === "streaming" ? bitrate(daemon.stats?.wire_bitrate_bps) : "paused"],
                  ["Session", duration(now - session.started_at_ms)],
                ]}
              />
              {canEnd && (
                <Button variant="contained" color="error" startIcon={<StopCircleOutlined />} onClick={onEnd}>
                  End session
                </Button>
              )}
            </Stack>
          </Stack>
        )}
      </CardContent>
    </Card>
  );
}

function Tile({ label, value, detail, trend, color }: { label: string; value: string; detail?: string; trend?: number[]; color?: string }) {
  return (
    <Card sx={{ flex: "1 1 200px", minWidth: 180 }}>
      <CardContent>
        <Typography variant="body2" color="text.secondary">
          {label}
        </Typography>
        <Typography variant="h5" sx={{ fontVariantNumeric: "tabular-nums", my: 0.5 }}>
          {value}
        </Typography>
        {detail && (
          <Typography variant="caption" color="text.secondary">
            {detail}
          </Typography>
        )}
        {trend && <Sparkline values={trend} color={color} />}
      </CardContent>
    </Card>
  );
}

function Performance({ stats, history }: { stats: StreamStats; history: StreamStats[] }) {
  const total = stats.stages.find((stage) => stage.id === "total");
  const slowest = Math.max(1, ...stats.stages.filter((stage) => stage.id !== "total").map((stage) => stage.p95_us));
  return (
    <Box sx={{ mb: 3 }}>
      <Typography variant="h6" sx={{ mb: 1.5 }}>
        Performance
      </Typography>
      <Stack direction="row" sx={{ gap: 2, flexWrap: "wrap", mb: 2 }}>
        <Tile
          label="Frame rate"
          value={`${stats.fps.toFixed(1)} fps`}
          detail={stats.stale_frames_dropped > 0 ? `${stats.stale_frames_dropped} stale frames skipped` : "frames delivered per second"}
          trend={history.map((s) => s.fps)}
          color="success.main"
        />
        <Tile
          label="Encoded bitrate"
          value={bitrate(stats.encoded_bitrate_bps)}
          detail={`Wire ${bitrate(stats.wire_bitrate_bps)}`}
          trend={history.map((s) => s.encoded_bitrate_bps)}
          color="primary.main"
        />
        <Tile
          label="Pipeline latency"
          value={micros(total?.p50_us)}
          detail={`p95 ${micros(total?.p95_us)} · max ${micros(total?.max_us)}`}
          trend={history.map((s) => s.stages.find((stage) => stage.id === "total")?.p50_us ?? 0)}
          color="info.main"
        />
        <Tile
          label="Transport overhead"
          value={stats.transport_overhead_percent == null ? "—" : `${stats.transport_overhead_percent.toFixed(1)} %`}
          detail={`${Math.round(stats.packets / (stats.window_ms / 1000)).toLocaleString()} packets/s${
            stats.failed_packets ? ` · ${stats.failed_packets} failed` : ""
          }`}
        />
      </Stack>
      <Card>
        <Table size="small" aria-label="Pipeline stages">
          <TableHead>
            <TableRow>
              <TableCell>Stage</TableCell>
              <TableCell align="right">Average</TableCell>
              <TableCell align="right">Median</TableCell>
              <TableCell align="right">p95</TableCell>
              <TableCell align="right">Max</TableCell>
              <TableCell sx={{ width: "30%" }} />
            </TableRow>
          </TableHead>
          <TableBody>
            {stats.stages.map((stage) => (
              <TableRow key={stage.id} sx={stage.id === "total" ? { "& td": { fontWeight: 600 } } : undefined}>
                <TableCell>{stage.label}</TableCell>
                <TableCell align="right" sx={{ fontVariantNumeric: "tabular-nums" }}>
                  {micros(stage.avg_us)}
                </TableCell>
                <TableCell align="right" sx={{ fontVariantNumeric: "tabular-nums" }}>
                  {micros(stage.p50_us)}
                </TableCell>
                <TableCell align="right" sx={{ fontVariantNumeric: "tabular-nums" }}>
                  {micros(stage.p95_us)}
                </TableCell>
                <TableCell align="right" sx={{ fontVariantNumeric: "tabular-nums" }}>
                  {micros(stage.max_us)}
                </TableCell>
                <TableCell>
                  {stage.id !== "total" && (
                    <LinearProgress
                      variant="determinate"
                      value={Math.min(100, (stage.p95_us / slowest) * 100)}
                      sx={{ height: 6, borderRadius: 3 }}
                      aria-label={`${stage.label} share`}
                    />
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </Card>
      <Typography variant="caption" color="text.secondary" sx={{ display: "block", mt: 1 }}>
        One-second windows from the video pipeline's own frame timings.
        {stats.samples_dropped > 0 && ` ${stats.samples_dropped} samples were skipped so the stream is never slowed.`}
      </Typography>
    </Box>
  );
}

function FramePacing({ capture, history, streamFps }: { capture: CaptureStats; history: StreamStats[]; streamFps?: number }) {
  const static_ = capture.source_fps === 0;
  return (
    <Box sx={{ mb: 3 }}>
      <Stack direction="row" sx={{ alignItems: "center", gap: 1.5, mb: 1.5 }}>
        <Typography variant="h6">Frame pacing</Typography>
        <Chip
          label={pacingLabels[capture.pacing]}
          size="small"
          color={capture.pacing === "vrr" ? "success" : capture.pacing === "mixed" ? "warning" : "default"}
          variant="outlined"
        />
        {capture.path && (
          <Chip
            label={pathLabels[capture.path.current]}
            size="small"
            color={capture.path.current === "composited" ? "warning" : capture.path.current === "mixed" ? "default" : "success"}
            variant="outlined"
          />
        )}
      </Stack>
      <Stack direction="row" sx={{ gap: 2, flexWrap: "wrap", mb: 1 }}>
        <Tile
          label="Game frame rate"
          value={static_ ? "Static" : `${capture.source_fps.toFixed(1)} fps`}
          detail={streamFps ? `stream limit ${streamFps} fps` : "new frames per second"}
          trend={history.map((s) => s.capture?.source_fps ?? 0)}
          color="success.main"
        />
        <Tile
          label="Frame time"
          value={static_ ? "—" : micros(capture.interval_p50_us)}
          detail={static_ ? "no new frames" : `p95 ${micros(capture.interval_p95_us)} · p99 ${micros(capture.interval_p99_us)} · max ${micros(capture.interval_max_us)}`}
          trend={history.map((s) => s.capture?.interval_p95_us ?? 0)}
          color="info.main"
        />
        <Tile
          label="Uneven frame times"
          value={static_ ? "—" : `${capture.uneven_percent.toFixed(1)} %`}
          detail={static_ ? "no new frames" : `changes over 2 ms · σ ${micros(capture.interval_stddev_us)}`}
          trend={history.map((s) => s.capture?.uneven_percent ?? 0)}
          color="warning.main"
        />
        <Tile
          label="Capture delay"
          value={micros(capture.content_age_p50_us)}
          detail={`p95 ${micros(capture.content_age_p95_us)} · new content to capture`}
        />
        {capture.path && <Tile label="Capture path" value={pathLabels[capture.path.current]} detail={pathDetail(capture.path)} />}
      </Stack>
      <Typography variant="caption" color="text.secondary" sx={{ display: "block" }}>
        {capture.pacing === "fixed"
          ? "Frames are captured on the stream's refresh clock. A client with VRR presentation (Pyrolight with VRR enabled) switches the host to VRR capture."
          : "Frames are captured as the application presents them, up to the stream's frame rate, and sent with their real frame times, which a VRR client paces the display by."}
      </Typography>
    </Box>
  );
}

const engineLabels: Record<GpuEngineStats["class"], string> = {
  graphics: "Graphics",
  compute: "Compute",
  video_encode: "Video encode",
  video_decode: "Video decode",
  video: "Video",
  copy: "Copy",
  other: "Other",
};

function megahertz(value: number | null): string | undefined {
  return value == null ? undefined : `${Math.round(value)} MHz`;
}

/// Capture GPU readings: device sensors, then GPU time per process group.
function GpuSection({ gpu, history }: { gpu: GpuStats; history: StreamStats[] }) {
  const tiles: { label: string; value: string; detail?: string; trend?: number[]; color?: string }[] = [];
  if (gpu.busy_percent != null) {
    tiles.push({
      label: "GPU busy",
      value: percent(gpu.busy_percent),
      detail: megahertz(gpu.core_clock_mhz) ? `core ${megahertz(gpu.core_clock_mhz)}` : "graphics and compute",
      trend: history.map((s) => s.gpu?.busy_percent ?? 0),
      color: "warning.main",
    });
  } else if (gpu.core_clock_mhz != null) {
    tiles.push({ label: "Core clock", value: megahertz(gpu.core_clock_mhz)!, detail: "this GPU reports no busy counter" });
  }
  if (gpu.encoder_busy_percent != null) {
    tiles.push({
      label: "Video encoder",
      value: percent(gpu.encoder_busy_percent),
      detail: "encode engine busy",
      trend: history.map((s) => s.gpu?.encoder_busy_percent ?? 0),
      color: "info.main",
    });
  }
  if (gpu.temperature_c != null) {
    tiles.push({
      label: "Temperature",
      value: `${gpu.temperature_c.toFixed(0)} °C`,
      detail: gpu.hotspot_c != null ? `hotspot ${gpu.hotspot_c.toFixed(0)} °C` : undefined,
    });
  }
  if (gpu.power_w != null) {
    tiles.push({
      label: "Power",
      value: `${gpu.power_w.toFixed(0)} W`,
      detail: gpu.power_limit_w != null ? `limit ${gpu.power_limit_w.toFixed(0)} W` : undefined,
    });
  }
  if (gpu.vram_used_bytes != null) {
    tiles.push({
      label: "Video memory",
      value: bytes(gpu.vram_used_bytes),
      detail: gpu.vram_total_bytes != null ? `of ${bytes(gpu.vram_total_bytes)}` : undefined,
    });
  }
  const rows = gpu.processes.flatMap((group) =>
    group.engines.length === 0
      ? [{ group, engine: null as GpuEngineStats | null }]
      : group.engines.map((engine) => ({ group, engine: engine as GpuEngineStats | null })),
  );
  return (
    <Box sx={{ mb: 3 }} aria-label="GPU">
      <Stack direction="row" sx={{ alignItems: "center", gap: 1.5, mb: 1.5 }}>
        <Typography variant="h6">GPU</Typography>
        <Chip label={gpu.driver} size="small" variant="outlined" />
      </Stack>
      {tiles.length > 0 && (
        <Stack direction="row" sx={{ gap: 2, flexWrap: "wrap", mb: 2 }}>
          {tiles.map((tile) => (
            <Tile key={tile.label} {...tile} />
          ))}
        </Stack>
      )}
      {rows.length > 0 && (
        <Card>
          <Table size="small" aria-label="GPU time by process">
            <TableHead>
              <TableRow>
                <TableCell>Process</TableCell>
                <TableCell>Engine</TableCell>
                <TableCell align="right">Busy</TableCell>
                <TableCell align="right">Video memory</TableCell>
                <TableCell sx={{ width: "30%" }} />
              </TableRow>
            </TableHead>
            <TableBody>
              {rows.map(({ group, engine }, index) => (
                <TableRow key={`${group.role}-${engine?.engine ?? "none"}`}>
                  <TableCell>{index === 0 || rows[index - 1].group !== group ? group.label || group.role : ""}</TableCell>
                  <TableCell>{engine ? `${engineLabels[engine.class] ?? engine.class} (${engine.engine})` : "No recent sample"}</TableCell>
                  <TableCell align="right" sx={{ fontVariantNumeric: "tabular-nums" }}>
                    {engine ? percent(engine.busy_percent) : "—"}
                  </TableCell>
                  <TableCell align="right" sx={{ fontVariantNumeric: "tabular-nums" }}>
                    {index === 0 || rows[index - 1].group !== group ? bytes(group.vram_bytes) : ""}
                  </TableCell>
                  <TableCell>
                    {engine && (
                      <LinearProgress
                        variant="determinate"
                        value={Math.min(100, engine.busy_percent)}
                        color={group.role === "application" ? "warning" : "primary"}
                        sx={{ height: 6, borderRadius: 3 }}
                        aria-label={`${group.label} ${engine.engine} busy`}
                      />
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </Card>
      )}
      <Typography variant="caption" color="text.secondary" sx={{ display: "block", mt: 1 }}>
        {gpu.source === "nvml" ? "Read through the NVIDIA driver (NVML)" : "Read from the kernel's GPU sensors and per-process DRM usage"} once
        per second. The application row covers every process the launched application started; Pyroshine's covers
        capture, composition, color conversion and encoding.
        {gpu.source === "nvml" && " The NVIDIA driver samples a process only while it is busy, so light work may show no sample."}
      </Typography>
    </Box>
  );
}

const QUALITY_LABELS: Record<string, string> = { standard: "Standard", high: "High", maximum: "Maximum" };

function audioQuality(audio: AudioDetails): string {
  if (!audio.quality) return "—";
  const level = QUALITY_LABELS[audio.quality] ?? audio.quality;
  return `${level} · ${audio.quality_requested ? "requested by the client" : "host default"}`;
}

function opusLayout(audio: AudioDetails): string {
  const streams = audio.opus_streams ?? 0;
  const coupled = audio.opus_coupled_streams ?? 0;
  if (audio.high_quality && audio.channels > 2) {
    return streams ? `High-quality surround · ${streams} mono streams` : "High-quality surround";
  }
  if (!streams) return "Standard";
  const mono = streams - coupled;
  const parts = [coupled ? `${coupled} stereo` : "", mono ? `${mono} mono` : ""].filter(Boolean);
  return `${streams} ${streams === 1 ? "stream" : "streams"} (${parts.join(" + ")})`;
}

function AudioCard({ session }: { session: SessionDetails }) {
  const audio = session.audio;
  return (
    <Card sx={{ flex: "1 1 300px" }}>
      <CardContent>
        <Typography variant="h6" sx={{ mb: 2 }}>
          Audio
        </Typography>
        {audio ? (
          <Facts
            rows={[
              ["Channels", `${channels(audio.channels)} · mask 0x${audio.channel_mask.toString(16)}`],
              ["Quality", audioQuality(audio)],
              ["Opus bitrate", bitrate(audio.opus_bitrate_bps)],
              ["Opus layout", opusLayout(audio)],
              ["Sample rate", audio.sample_rate_hz ? `${audio.sample_rate_hz / 1000} kHz` : "—"],
              ["Packet duration", `${audio.packet_duration_ms} ms`],
              ["Encryption", audio.encrypted ? "AES-128-CBC" : "Off"],
            ]}
          />
        ) : (
          <Typography color="text.secondary">Negotiating {channels(session.requested.audio_channels).toLowerCase()}…</Typography>
        )}
      </CardContent>
    </Card>
  );
}

const inputLabels: Record<string, string> = {
  libei: "Keyboard and pointer (libei)",
  portal: "Keyboard and pointer (portal)",
  none: "View-only",
};

/** Mode and, in Desktop Mode, what of the desktop is captured and controlled. */
function captureRows(capture: SessionDetails["capture"]): [string, string][] {
  if (!capture) {
    return [];
  }
  const rows: [string, string][] = [["Mode", capture.mode_label]];
  if (capture.mode === "desktop") {
    if (capture.output) {
      const refresh = capture.output.refresh_millihertz ? ` @ ${(capture.output.refresh_millihertz / 1000).toFixed(0)} Hz` : "";
      rows.push(["Captured output", `${capture.output.width} × ${capture.output.height}${refresh}`]);
    }
    rows.push(["Desktop", capture.current_desktop || capture.desktop || "—"]);
    rows.push(["Remote input", inputLabels[capture.input ?? "none"] ?? capture.input ?? "—"]);
    rows.push(["Permission", capture.consent === "restored" ? "Saved permission offered" : "Asked on the desktop"]);
  }
  rows.push(["Captured content", capture.dynamic_range]);
  rows.push(["Capture backend", `${capture.backend === "desktop" ? "XDG Desktop Portal" : "Isolated compositor"} on ${capture.render_node}`]);
  return rows;
}

function StreamDetails({ session }: { session: SessionDetails }) {
  const video = session.video;
  return (
    <Stack direction={{ xs: "column", md: "row" }} sx={{ gap: 2, flexWrap: "wrap" }}>
      <Card sx={{ flex: "1 1 300px" }}>
        <CardContent>
          <Typography variant="h6" sx={{ mb: 2 }}>
            Video
          </Typography>
          {video ? (
            <Facts
              rows={[
                ["Codec", video.pyrowave_dialect ? `${video.codec_label} (${video.pyrowave_dialect.replaceAll("_", " ")})` : video.codec_label],
                ["Resolution", `${video.width} × ${video.height}`],
                ["Refresh rate", `${video.fps} Hz`],
                ["Chroma", video.chroma],
                ["Bit depth", `${video.bit_depth}-bit`],
                ["Dynamic range", video.dynamic_range],
                ["Color", `${video.primaries} primaries · ${video.transfer} · ${video.matrix} · ${video.range} range`],
                ...(video.encoder ? [["Encoder", `${video.encoder.backend_label} on ${video.encoder.gpu || "the capture GPU"}`] as [string, string]] : []),
                ["Target bitrate", bitrate(video.bitrate_bps)],
                ["Packet size", `${video.packet_size} bytes`],
                ["Minimum FEC", `${video.minimum_fec_packets} packets per frame`],
                ["Encryption", video.encrypted ? "AES-128-GCM" : "Off"],
              ]}
            />
          ) : (
            <Typography color="text.secondary">Negotiating…</Typography>
          )}
        </CardContent>
      </Card>
      <AudioCard session={session} />
      <Card sx={{ flex: "1 1 300px" }}>
        <CardContent>
          <Typography variant="h6" sx={{ mb: 2 }}>
            Session
          </Typography>
          <Facts
            rows={[
              ...captureRows(session.capture),
              ["Foreground application", session.foreground_application?.title ?? "—"],
              ...(session.steam_game
                ? [["Steam game", `${steamGameStates[session.steam_game.state]} (app ${session.steam_game.app_id})`] as [string, string]]
                : []),
              ["Moonlight application", session.application.title],
              ["Application ID", String(session.application.id)],
              ["Client", session.client_address],
              ["Requested mode", `${session.requested.width} × ${session.requested.height} @ ${session.requested.refresh_rate} Hz${session.requested.hdr ? " · HDR" : ""}`],
              ["Started", new Date(session.started_at_ms).toLocaleTimeString()],
            ]}
          />
        </CardContent>
      </Card>
    </Stack>
  );
}

export function DesktopPanel({ status, busy, onAuthorize, onRestore }: { status: DesktopStatus; busy: boolean; onAuthorize: () => void; onRestore: () => void }) {
  const now = useNow();
  if (status.phase === "disabled") return null;
  const active = ["active", "grace", "preparing", "restoring", "recovery_required"].includes(status.phase);
  const remaining = status.grace_deadline_ms == null ? null : Math.max(0, Math.ceil((status.grace_deadline_ms - now) / 1000));
  return <Card sx={{ mb: 3 }}><CardContent>
    <Typography variant="h6">Desktop Mode</Typography>
    {status.reason && <Alert severity={status.phase === "recovery_required" ? "error" : "info"} sx={{ my: 1 }}>{status.reason}</Alert>}
    {status.mode && active && <Typography>{status.mode.width} × {status.mode.height} @ {status.mode.refresh_millihertz / 1000} Hz · SDR virtual display</Typography>}
    {remaining != null && <Typography>Physical displays restore in {remaining} seconds unless the client reconnects.</Typography>}
    {status.audio_remote_only && <Typography color="text.secondary">Desktop sound is routed to the client. Local audio routing restores when Desktop Mode ends.</Typography>}
    {(status.supported || active) && <Stack direction="row" sx={{ gap: 1, mt: 2 }}>
      <Button disabled={busy || active || !status.supported} onClick={onAuthorize}>Authorize remote input</Button>
      <Button disabled={busy || !active} onClick={onRestore} variant="outlined">Restore desktop</Button>
    </Stack>}
  </CardContent></Card>;
}

export function DashboardPage({ navigate }: { navigate: (to: string) => void }) {
  const daemon = useDaemon();
  const [confirm, setConfirm] = useState(false);
  const [ending, setEnding] = useState(false);
  const [desktop, setDesktop] = useState<DesktopStatus | null>(null);
  const [desktopBusy, setDesktopBusy] = useState(false);
  useEffect(() => {
    if (!daemon.connected) return;
    let active = true;
    const poll = () => { void api.desktopStatus().then((status) => { if (active) setDesktop(status); }).catch(() => {}); };
    poll(); const timer = setInterval(poll, 2000);
    return () => { active = false; clearInterval(timer); };
  }, [daemon.connected]);
  const desktopAction = async (restore: boolean) => {
    setDesktopBusy(true);
    try {
      if (restore) { setDesktop(await api.restoreDesktop()); daemon.notify("success", "Desktop configuration restored."); }
      else { await api.authorizeDesktop(); daemon.notify("success", "Remote input authorized on the host."); }
    } catch (error) { daemon.notify("error", errorMessage(error)); }
    finally { setDesktopBusy(false); }
  };
  const session = daemon.session?.session;
  const lastStop = daemon.session?.last_stop;

  const end = async () => {
    setEnding(true);
    try {
      await api.endSession();
      daemon.notify("success", "Session ended.");
    } catch (error) {
      daemon.notify("error", errorMessage(error));
    } finally {
      setEnding(false);
      setConfirm(false);
    }
  };

  return (
    <>
      <PageHeader title="Dashboard" subtitle={daemon.server ? `${daemon.server.name} · Pyroshine ${daemon.server.version}` : undefined} />
      <Hero onEnd={() => setConfirm(true)} navigate={navigate} />
      {desktop && <DesktopPanel status={desktop} busy={desktopBusy} onAuthorize={() => void desktopAction(false)} onRestore={() => void desktopAction(true)} />}
      {daemon.session?.phase === "idle" && lastStop?.unexpected && (
        <Alert severity="warning" sx={{ mb: 3 }}>
          The last session ended unexpectedly at {new Date(lastStop.at_ms).toLocaleTimeString()}: {lastStop.detail ?? lastStop.message}
          {lastStop.hint && <Hint text={lastStop.hint} />}
        </Alert>
      )}
      {daemon.session?.phase === "streaming" && daemon.stats?.findings && daemon.stats.epoch === session?.epoch && (
        <Diagnosis findings={daemon.stats.findings} />
      )}
      {daemon.session?.phase === "streaming" && daemon.stats && daemon.stats.epoch === session?.epoch && (
        <Performance stats={daemon.stats} history={daemon.history} />
      )}
      {daemon.session?.phase === "streaming" && daemon.stats?.capture && daemon.stats.epoch === session?.epoch && (
        <FramePacing capture={daemon.stats.capture} history={daemon.history} streamFps={session?.video?.fps} />
      )}
      {daemon.session?.phase === "streaming" && daemon.stats?.gpu && daemon.stats.epoch === session?.epoch && (
        <GpuSection gpu={daemon.stats.gpu} history={daemon.history} />
      )}
      {daemon.session?.phase === "streaming" && !daemon.stats && (
        <Typography color="text.secondary" sx={{ mb: 3 }}>
          Collecting performance statistics…
        </Typography>
      )}
      {session && <StreamDetails session={session} />}
      <ConfirmDialog
        open={confirm}
        title="End the session?"
        confirm={ending ? "Ending…" : "End session"}
        destructive
        busy={ending}
        onConfirm={() => void end()}
        onClose={() => setConfirm(false)}
      >
        {session?.capture?.mode === "desktop" ? "This ends Desktop Mode and restores displays and audio routing. Your desktop applications remain open." : <>This closes <strong>{session?.application.title ?? "the application"}</strong> and disconnects the client. Unsaved game progress may be lost.</>}
      </ConfirmDialog>
    </>
  );
}
