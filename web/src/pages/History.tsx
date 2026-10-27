import { useCallback, useEffect, useMemo, useState } from "react";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import ButtonBase from "@mui/material/ButtonBase";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Chip from "@mui/material/Chip";
import CircularProgress from "@mui/material/CircularProgress";
import Divider from "@mui/material/Divider";
import FormControlLabel from "@mui/material/FormControlLabel";
import Stack from "@mui/material/Stack";
import Switch from "@mui/material/Switch";
import Table from "@mui/material/Table";
import TableBody from "@mui/material/TableBody";
import TableCell from "@mui/material/TableCell";
import TableHead from "@mui/material/TableHead";
import TableRow from "@mui/material/TableRow";
import Typography from "@mui/material/Typography";
import ArrowBack from "@mui/icons-material/ArrowBack";
import DeleteSweepOutlined from "@mui/icons-material/DeleteSweepOutlined";
import Refresh from "@mui/icons-material/Refresh";
import { api, errorMessage } from "../api/client";
import type { HistoryEntry, HistoryPage } from "../api/types";
import { ConfirmDialog, Facts, Hint, PageHeader } from "../components/common";
import { useDaemon } from "../state/daemon";
import { bitrate, channels, duration, micros, percent, relative } from "../util/format";

const PAGE = 50;

function when(ms: number): string {
  return new Date(ms).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

function length(entry: HistoryEntry): string {
  return entry.ended_at_ms ? duration(entry.ended_at_ms - entry.started_at_ms) : "running";
}

function mode(entry: HistoryEntry): string | null {
  const video = entry.video;
  if (video) return `${video.width}×${video.height} @ ${video.fps} Hz`;
  const requested = entry.requested;
  return requested ? `${requested.width}×${requested.height} @ ${requested.refresh_rate} Hz` : null;
}

function client(entry: HistoryEntry): string {
  return entry.client_label ?? entry.client_address ?? "Not recorded";
}

/// The session's outcome as a chip: how it ended, highlighted when unexpected.
export function StopChip({ entry }: { entry: HistoryEntry }) {
  if (!entry.ended_at_ms) return <Chip size="small" color="success" label="Running" />;
  const stop = entry.stop;
  if (!stop) return <Chip size="small" label="Ended" />;
  return (
    <Chip
      size="small"
      variant={stop.unexpected ? "filled" : "outlined"}
      color={stop.unexpected ? "error" : "default"}
      label={stop.reason === "interrupted" ? "Interrupted" : stop.unexpected ? "Ended unexpectedly" : "Ended"}
    />
  );
}

function Row({ entry, onOpen }: { entry: HistoryEntry; onOpen: () => void }) {
  const now = Date.now();
  return (
    <ButtonBase onClick={onOpen} sx={{ display: "block", textAlign: "left", width: "100%", px: { xs: 2, sm: 3 }, py: 1.75 }}>
      <Stack direction={{ xs: "column", sm: "row" }} sx={{ gap: { xs: 0.75, sm: 2 }, alignItems: { sm: "center" } }}>
        <Box sx={{ flex: 1, minWidth: 0 }}>
          <Typography sx={{ fontWeight: 500 }} noWrap>
            {entry.foreground_application ?? entry.application}
            {entry.foreground_application && (
              <Typography component="span" variant="body2" color="text.secondary">
                {" "}
                · via {entry.application}
              </Typography>
            )}
          </Typography>
          <Typography variant="body2" color="text.secondary" noWrap>
            {client(entry)} · {relative(entry.started_at_ms, now)} · {length(entry)}
          </Typography>
        </Box>
        <Stack direction="row" sx={{ gap: 0.75, flexWrap: "wrap", alignItems: "center" }}>
          {mode(entry) && <Chip size="small" variant="outlined" label={mode(entry)} />}
          {entry.video && <Chip size="small" variant="outlined" label={entry.video.codec_label} />}
          {entry.video?.hdr && <Chip size="small" variant="outlined" label="HDR" />}
          {entry.reconnects > 0 && <Chip size="small" variant="outlined" label={`${entry.reconnects} reconnect${entry.reconnects === 1 ? "" : "s"}`} />}
          <StopChip entry={entry} />
        </Stack>
      </Stack>
    </ButtonBase>
  );
}

/// Differences from the previous session of the same application.
function Comparison({ entry, previous }: { entry: HistoryEntry; previous: HistoryEntry | undefined }) {
  const now = entry.performance;
  const then = previous?.performance;
  if (!now || !then || !previous) return null;
  const encode = (summary: typeof now) => summary.stages.find((stage) => stage.id === "encode")?.avg_us;
  const rows: [string, string, string][] = [
    ["Average frame rate", `${now.fps_avg.toFixed(1)} fps`, `${then.fps_avg.toFixed(1)} fps`],
    ["Slowest second", `${now.fps_min.toFixed(0)} fps`, `${then.fps_min.toFixed(0)} fps`],
    ["Wire bitrate", bitrate(now.wire_bitrate_avg_bps), bitrate(then.wire_bitrate_avg_bps)],
    ["Encode time (average)", micros(encode(now)), micros(encode(then))],
    ["GPU busy", percent(now.gpu_busy_avg_percent), percent(then.gpu_busy_avg_percent)],
  ];
  return (
    <Box>
      <Typography variant="subtitle2" sx={{ mb: 1 }}>
        Compared with the previous {entry.application} session ({when(previous.started_at_ms)})
      </Typography>
      <Table size="small">
        <TableHead>
          <TableRow>
            <TableCell />
            <TableCell>This session</TableCell>
            <TableCell>Previous</TableCell>
          </TableRow>
        </TableHead>
        <TableBody>
          {rows.map(([label, a, b]) => (
            <TableRow key={label}>
              <TableCell component="th" sx={{ color: "text.secondary" }}>
                {label}
              </TableCell>
              <TableCell>{a}</TableCell>
              <TableCell>{b}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </Box>
  );
}

export function SessionDetail({ entry, previous }: { entry: HistoryEntry; previous?: HistoryEntry }) {
  const performance = entry.performance;
  const video = entry.video;
  return (
    <Stack sx={{ gap: 3 }}>
      <Facts
        rows={[
          ["Application", entry.foreground_application ? `${entry.foreground_application} (launched as ${entry.application})` : entry.application],
          ["Client", entry.client_label ? `${entry.client_label} (${entry.client_address ?? "address not recorded"})` : client(entry)],
          ["Started", when(entry.started_at_ms)],
          ["Ended", entry.ended_at_ms ? when(entry.ended_at_ms) : "Still running"],
          ["Length", length(entry)],
          [
            "Requested",
            entry.requested
              ? `${entry.requested.width}×${entry.requested.height} @ ${entry.requested.refresh_rate} Hz${entry.requested.hdr ? ", HDR" : ""}`
              : "—",
          ],
          [
            "Stream",
            video
              ? `${video.width}×${video.height} @ ${video.fps} fps · ${video.codec_label} ${video.chroma} ${video.bit_depth}-bit · ${video.hdr ? "HDR" : "SDR"} · ${bitrate(video.bitrate_bps)} target`
              : "Never streamed",
          ],
          ["Encoder", video?.encoder ? `${video.encoder}${video.gpu ? ` on ${video.gpu}` : ""}` : "—"],
          ["Audio", entry.audio_channels ? channels(entry.audio_channels) : "—"],
          ["Disconnects / resumes", `${entry.disconnects} / ${entry.reconnects}`],
        ]}
      />

      {entry.stop && (
        <Alert severity={entry.stop.unexpected ? "error" : "info"}>
          <Typography variant="body2" sx={{ fontWeight: 500 }}>
            {entry.stop.message}
          </Typography>
          {entry.stop.detail && <Typography variant="body2">{entry.stop.detail}</Typography>}
          {entry.stop.hint && <Hint text={entry.stop.hint} />}
        </Alert>
      )}

      {entry.events.length > 0 && (
        <Box>
          <Typography variant="subtitle2" sx={{ mb: 1 }}>
            Timeline
          </Typography>
          <Stack sx={{ gap: 0.5 }}>
            {entry.events.map((event, index) => (
              <Typography key={index} variant="body2">
                <Box component="span" sx={{ color: "text.secondary", fontVariantNumeric: "tabular-nums" }}>
                  {duration(event.at_ms - entry.started_at_ms)}
                </Box>{" "}
                {event.kind === "disconnected"
                  ? "Client disconnected; the application kept running"
                  : event.kind === "reconnected"
                    ? "Client resumed the session"
                    : (event.detail ?? "Session ended")}
              </Typography>
            ))}
          </Stack>
        </Box>
      )}

      <Divider />
      <Box>
        <Typography variant="subtitle2">Performance on this host</Typography>
        <Typography variant="body2" color="text.secondary" sx={{ mb: 1.5 }}>
          Measured by Pyroshine while frames were sent. What the client displayed (its decode time, network loss on
          the way) is not visible to the host and not included.
        </Typography>
        {performance ? (
          <Stack sx={{ gap: 2 }}>
            <Facts
              rows={[
                ["Streamed", `${duration(performance.streaming_ms)} (${performance.windows} one-second windows)`],
                ["Frame rate", `${performance.fps_avg.toFixed(1)} fps average · ${performance.fps_min.toFixed(0)} fps slowest second`],
                ["Bitrate", `${bitrate(performance.encoded_bitrate_avg_bps)} encoded · ${bitrate(performance.wire_bitrate_avg_bps)} on the wire`],
                ["Dropped", `${performance.stale_frames_dropped} stale frames · ${performance.failed_packets} failed and ${performance.discarded_packets} discarded packets`],
                ["GPU", `${percent(performance.gpu_busy_avg_percent)} busy${performance.encoder_busy_avg_percent != null ? `, encoder ${percent(performance.encoder_busy_avg_percent)}` : ""}${performance.gpu_temperature_max_c != null ? `, up to ${performance.gpu_temperature_max_c.toFixed(0)} °C` : ""}`],
              ]}
            />
            {performance.stages.length > 0 && (
              <Table size="small" aria-label="Pipeline stages">
                <TableHead>
                  <TableRow>
                    <TableCell>Stage</TableCell>
                    <TableCell align="right">Average</TableCell>
                    <TableCell align="right">Worst p95</TableCell>
                  </TableRow>
                </TableHead>
                <TableBody>
                  {performance.stages.map((stage) => (
                    <TableRow key={stage.id}>
                      <TableCell>{stage.label}</TableCell>
                      <TableCell align="right">{micros(stage.avg_us)}</TableCell>
                      <TableCell align="right">{micros(stage.p95_max_us)}</TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
            {performance.findings.length > 0 && (
              <Box>
                <Typography variant="subtitle2" sx={{ mb: 0.5 }}>
                  Recurring findings
                </Typography>
                {performance.findings.map((finding) => (
                  <Typography key={finding.id} variant="body2">
                    {finding.title}{" "}
                    <Box component="span" sx={{ color: "text.secondary" }}>
                      ({finding.windows} of {performance.windows} windows)
                    </Box>
                  </Typography>
                ))}
              </Box>
            )}
            <Comparison entry={entry} previous={previous} />
          </Stack>
        ) : (
          <Typography variant="body2" color="text.secondary">
            No statistics were collected: the session never streamed a complete one-second window.
          </Typography>
        )}
      </Box>
    </Stack>
  );
}

export function HistoryPageView({ selected, navigate }: { selected: number | null; navigate: (to: string) => void }) {
  const { lastHistoryEntry, notify } = useDaemon();
  const [page, setPage] = useState<HistoryPage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [problemsOnly, setProblemsOnly] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);
  const [detail, setDetail] = useState<HistoryEntry | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setPage(await api.history(PAGE));
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // A session that just ended appears without reloading.
  useEffect(() => {
    if (!lastHistoryEntry) return;
    setPage((current) =>
      current && !current.entries.some((entry) => entry.id === lastHistoryEntry.id)
        ? { ...current, total: current.total + 1, entries: [lastHistoryEntry, ...current.entries] }
        : current,
    );
  }, [lastHistoryEntry]);

  // A linked session that is not on the loaded page is fetched on its own.
  useEffect(() => {
    if (selected === null) {
      setDetail(null);
      return;
    }
    const loaded = page?.entries.find((entry) => entry.id === selected);
    if (loaded) {
      setDetail(loaded);
      return;
    }
    if (!page) return;
    api.historyEntry(selected).then(setDetail, (e) => {
      notify("warning", errorMessage(e));
      navigate("history");
    });
  }, [selected, page, notify, navigate]);

  const loadOlder = async () => {
    if (!page || page.entries.length === 0) return;
    setLoading(true);
    try {
      const older = await api.history(PAGE, page.entries[page.entries.length - 1].id);
      setPage({ ...older, entries: [...page.entries, ...older.entries] });
    } catch (e) {
      notify("error", errorMessage(e));
    } finally {
      setLoading(false);
    }
  };

  const clear = async () => {
    setClearing(true);
    try {
      await api.clearHistory();
      setConfirmClear(false);
      notify("success", "Session history cleared.");
      await load();
    } catch (e) {
      notify("error", errorMessage(e));
    } finally {
      setClearing(false);
    }
  };

  const entries = useMemo(
    () => (page?.entries ?? []).filter((entry) => !problemsOnly || entry.stop?.unexpected),
    [page, problemsOnly],
  );

  if (detail) {
    const all = page?.entries ?? [];
    const previous = all.find(
      (entry) => entry.application === detail.application && entry.started_at_ms < detail.started_at_ms && entry.performance,
    );
    return (
      <>
        <PageHeader
          title={detail.foreground_application ?? detail.application}
          subtitle={`Session of ${when(detail.started_at_ms)}`}
          actions={
            <Button startIcon={<ArrowBack />} onClick={() => navigate("history")}>
              All sessions
            </Button>
          }
        />
        <Card>
          <CardContent sx={{ p: { xs: 2, sm: 3 } }}>
            <SessionDetail entry={detail} previous={previous} />
          </CardContent>
        </Card>
        {detail.stop?.unexpected && (
          <Typography variant="body2" color="text.secondary" sx={{ mt: 2 }}>
            Reporting a problem? A support bundle from <b>Diagnostics</b> includes the service log and the last two minutes
            of statistics.{" "}
            <Button size="small" onClick={() => navigate("diagnostics")}>
              Open Diagnostics
            </Button>
          </Typography>
        )}
      </>
    );
  }

  return (
    <>
      <PageHeader
        title="Session history"
        subtitle="Past sessions, how they ended and how they performed on this host."
        actions={
          <>
            <Button variant="outlined" startIcon={loading ? <CircularProgress size={16} /> : <Refresh />} disabled={loading} onClick={() => void load()}>
              Refresh
            </Button>
            <Button
              variant="outlined"
              color="error"
              startIcon={<DeleteSweepOutlined />}
              disabled={!page?.available || page.total === 0}
              onClick={() => setConfirmClear(true)}
            >
              Clear
            </Button>
          </>
        }
      />
      {error && <Alert severity="error" sx={{ mb: 2 }}>{error}</Alert>}
      {page && !page.available && (
        <Alert severity="info" sx={{ mb: 2 }}>
          {page.error ?? "Session history is not recorded."} Turn it on in <b>Settings → Session history</b>.
        </Alert>
      )}
      {page?.available && page.error && (
        <Alert severity="warning" sx={{ mb: 2 }}>
          {page.error}
        </Alert>
      )}
      {page?.available && (
        <Stack direction="row" sx={{ alignItems: "center", justifyContent: "space-between", mb: 1, flexWrap: "wrap", gap: 1 }}>
          <Typography variant="body2" color="text.secondary">
            {page.total} session{page.total === 1 ? "" : "s"} · kept for {page.retention_days} days (at most {page.max_sessions})
          </Typography>
          <FormControlLabel
            control={<Switch checked={problemsOnly} onChange={(event) => setProblemsOnly(event.target.checked)} />}
            label="Only unexpected stops"
          />
        </Stack>
      )}
      {page?.available && (
        <Card>
          {entries.length === 0 ? (
            <CardContent sx={{ p: 3 }}>
              <Typography color="text.secondary">
                {problemsOnly ? "No session ended unexpectedly." : "No sessions yet. Finished sessions appear here."}
              </Typography>
            </CardContent>
          ) : (
            entries.map((entry, index) => (
              <Box key={entry.id}>
                {index > 0 && <Divider />}
                <Row entry={entry} onOpen={() => navigate(`history?session=${entry.id}`)} />
              </Box>
            ))
          )}
        </Card>
      )}
      {page && page.entries.length < page.total && (
        <Stack sx={{ alignItems: "center", mt: 2 }}>
          <Button disabled={loading} onClick={() => void loadOlder()}>
            Show older sessions
          </Button>
        </Stack>
      )}
      <ConfirmDialog
        open={confirmClear}
        title="Clear the session history?"
        confirm="Clear history"
        destructive
        busy={clearing}
        onConfirm={() => void clear()}
        onClose={() => setConfirmClear(false)}
      >
        Every recorded session is removed. Streaming and pairing are not affected.
      </ConfirmDialog>
    </>
  );
}
