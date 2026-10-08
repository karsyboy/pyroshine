import { useState } from "react";
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
import { call, errorMessage } from "../api/bridge";
import type { CaptureStats, SessionDetails, SessionPhase, StreamStats } from "../api/types";
import { ConfirmDialog, Facts, PageHeader, StatusDot, phaseLabels } from "../components/common";
import { Sparkline } from "../components/Sparkline";
import { useDaemon, useNow } from "../state/daemon";
import { bitrate, channels, duration, micros } from "../util/format";

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

const pacingLabels: Record<CaptureStats["pacing"], string> = {
  vrr: "VRR capture",
  fixed: "Fixed refresh",
  mixed: "Changing",
};

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
                  Client disconnected — waiting for it to reconnect. {session.application.title} keeps running until you
                  end the session or resume from Moonlight.
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
      </Stack>
      <Typography variant="caption" color="text.secondary" sx={{ display: "block" }}>
        {capture.pacing === "fixed"
          ? "Frames are captured on the stream's refresh clock. A client with VRR presentation (Pyrolight with VRR enabled) switches the host to VRR capture."
          : "Frames are captured as the application presents them, up to the stream's frame rate, and sent with their real frame times, which a VRR client paces the display by."}
      </Typography>
    </Box>
  );
}

function StreamDetails({ session }: { session: SessionDetails }) {
  const video = session.video;
  const audio = session.audio;
  return (
    <Stack direction={{ xs: "column", md: "row" }} sx={{ gap: 2 }}>
      <Card sx={{ flex: 1 }}>
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
      <Card sx={{ flex: 1 }}>
        <CardContent>
          <Typography variant="h6" sx={{ mb: 2 }}>
            Session
          </Typography>
          <Facts
            rows={[
              ["Foreground application", session.foreground_application?.title ?? "—"],
              ["Moonlight application", session.application.title],
              ["Application ID", String(session.application.id)],
              ["Client", session.client_address],
              ["Requested mode", `${session.requested.width} × ${session.requested.height} @ ${session.requested.refresh_rate} Hz${session.requested.hdr ? " · HDR" : ""}`],
              ["Audio", audio ? `${channels(audio.channels)} · Opus ${bitrate(audio.opus_bitrate_bps)} · ${audio.packet_duration_ms} ms packets` : channels(session.requested.audio_channels)],
              ["Audio encryption", audio ? (audio.encrypted ? "AES-128-CBC" : "Off") : "—"],
              ["Started", new Date(session.started_at_ms).toLocaleTimeString()],
            ]}
          />
        </CardContent>
      </Card>
    </Stack>
  );
}

export function DashboardPage({ navigate }: { navigate: (to: string) => void }) {
  const daemon = useDaemon();
  const [confirm, setConfirm] = useState(false);
  const [ending, setEnding] = useState(false);
  const session = daemon.session?.session;
  const lastStop = daemon.session?.last_stop;

  const end = async () => {
    setEnding(true);
    try {
      await call("end_session");
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
      {daemon.session?.phase === "idle" && lastStop?.unexpected && (
        <Alert severity="warning" sx={{ mb: 3 }}>
          The last session ended unexpectedly at {new Date(lastStop.at_ms).toLocaleTimeString()}: {lastStop.message}
        </Alert>
      )}
      {daemon.session?.phase === "streaming" && daemon.stats && daemon.stats.epoch === session?.epoch && (
        <Performance stats={daemon.stats} history={daemon.history} />
      )}
      {daemon.session?.phase === "streaming" && daemon.stats?.capture && daemon.stats.epoch === session?.epoch && (
        <FramePacing capture={daemon.stats.capture} history={daemon.history} streamFps={session?.video?.fps} />
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
        This closes <strong>{session?.application.title ?? "the application"}</strong> and disconnects the client, the same
        as quitting the app from Moonlight. Unsaved game progress may be lost.
      </ConfirmDialog>
    </>
  );
}
