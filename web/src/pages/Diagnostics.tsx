import { useState } from "react";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Chip from "@mui/material/Chip";
import Dialog from "@mui/material/Dialog";
import DialogActions from "@mui/material/DialogActions";
import DialogContent from "@mui/material/DialogContent";
import DialogTitle from "@mui/material/DialogTitle";
import Stack from "@mui/material/Stack";
import Table from "@mui/material/Table";
import TableBody from "@mui/material/TableBody";
import TableCell from "@mui/material/TableCell";
import TableRow from "@mui/material/TableRow";
import TextField from "@mui/material/TextField";
import Typography from "@mui/material/Typography";
import CheckCircle from "@mui/icons-material/CheckCircle";
import Archive from "@mui/icons-material/Archive";
import ContentCopy from "@mui/icons-material/ContentCopy";
import ErrorIcon from "@mui/icons-material/Error";
import InfoOutlined from "@mui/icons-material/InfoOutlined";
import Refresh from "@mui/icons-material/Refresh";
import Warning from "@mui/icons-material/Warning";
import { api, download, errorKind, errorMessage } from "../api/client";
import type { HealthCheck, HealthReport, ServerEvent } from "../api/types";
import { Facts, Hint, PageHeader } from "../components/common";
import { RestartButton } from "../components/Restart";
import { useDaemon, useNow } from "../state/daemon";
import { duration, relative } from "../util/format";

const outcomeIcons = {
  passed: <CheckCircle color="success" fontSize="small" />,
  warning: <Warning color="warning" fontSize="small" />,
  failed: <ErrorIcon color="error" fontSize="small" />,
};

const levelIcons = {
  info: <InfoOutlined color="info" fontSize="small" />,
  warning: <Warning color="warning" fontSize="small" />,
  error: <ErrorIcon color="error" fontSize="small" />,
};

const outcomeOrder = { failed: 0, warning: 1, passed: 2 };

/// Problems first, in the daemon's order within each outcome.
export function orderedChecks(checks: HealthCheck[]): HealthCheck[] {
  return checks
    .map((check, index) => ({ check, index }))
    .sort((a, b) => outcomeOrder[a.check.outcome] - outcomeOrder[b.check.outcome] || a.index - b.index)
    .map(({ check }) => check);
}

function HealthSummary({ health }: { health: HealthReport }) {
  const failed = health.checks.filter((check) => check.outcome === "failed").length;
  const warnings = health.checks.filter((check) => check.outcome === "warning").length;
  if (failed + warnings === 0) return <Chip label="All passed" color="success" size="small" variant="outlined" />;
  return (
    <Chip
      label={[failed && `${failed} failed`, warnings && `${warnings} ${warnings === 1 ? "warning" : "warnings"}`].filter(Boolean).join(" · ")}
      color={failed ? "error" : "warning"}
      size="small"
      variant="outlined"
    />
  );
}

function HealthCard({ health, running, now }: { health: HealthReport | null; running: boolean; now: number }) {
  return (
    <Card>
      <CardContent>
        <Stack direction="row" sx={{ alignItems: "center", gap: 1.5, mb: 1 }}>
          <Typography variant="h6">Host checks</Typography>
          {health && <HealthSummary health={health} />}
        </Stack>
        {!health ? (
          <Alert severity="info">
            Skipped at startup (--no-health-check). Run the checks to see whether this host can stream.
          </Alert>
        ) : (
          <>
            <Typography variant="body2" color="text.secondary" sx={{ mb: 1 }}>
              {health.rechecked ? "Checked" : "Checked at startup"}
              {health.checked_at_ms ? ` ${relative(health.checked_at_ms, now)}` : ""}
              {running ? " · checking again…" : ""}
            </Typography>
            <Table size="small" aria-label="Health checks">
              <TableBody>
                {orderedChecks(health.checks).map((check, index) => (
                  <TableRow key={`${check.id ?? check.name}-${index}`} sx={{ verticalAlign: "top" }}>
                    <TableCell sx={{ width: 32, pr: 0 }}>{outcomeIcons[check.outcome]}</TableCell>
                    <TableCell sx={{ fontWeight: 500, whiteSpace: "nowrap" }}>{check.name}</TableCell>
                    <TableCell>
                      <Typography variant="body2" sx={{ overflowWrap: "anywhere", whiteSpace: "pre-line" }}>
                        {check.message}
                      </Typography>
                      {check.hint && <Hint text={check.hint} />}
                    </TableCell>
                    <TableCell align="right" sx={{ color: "text.secondary", whiteSpace: "nowrap" }}>
                      {check.duration_ms} ms
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </>
        )}
      </CardContent>
    </Card>
  );
}

function EventsCard({ events, now }: { events: ServerEvent[]; now: number }) {
  const newest = [...events].reverse();
  return (
    <Card>
      <CardContent>
        <Typography variant="h6" sx={{ mb: 1 }}>
          Recent problems
        </Typography>
        {newest.length === 0 ? (
          <Typography variant="body2" color="text.secondary">
            No rejected launches, failed negotiations or unexpected stops since Pyroshine started.
          </Typography>
        ) : (
          <Stack sx={{ gap: 2 }} aria-label="Recent problems">
            {newest.map((event, index) => (
              <Stack key={`${event.at_ms}-${index}`} direction="row" sx={{ gap: 1.5, alignItems: "flex-start" }}>
                <Box sx={{ mt: 0.25 }}>{levelIcons[event.level]}</Box>
                <Box sx={{ minWidth: 0, flexGrow: 1 }}>
                  <Typography variant="body2" sx={{ overflowWrap: "anywhere" }}>
                    {event.message}
                  </Typography>
                  {event.hint && <Hint text={event.hint} />}
                </Box>
                <Typography variant="caption" color="text.secondary" sx={{ whiteSpace: "nowrap" }} title={new Date(event.at_ms).toLocaleString()}>
                  {relative(event.at_ms, now)}
                </Typography>
              </Stack>
            ))}
          </Stack>
        )}
      </CardContent>
    </Card>
  );
}

/// The report as text the operator can copy when the clipboard is unavailable.
function ReportDialog({ report, onClose }: { report: string | null; onClose: () => void }) {
  return (
    <Dialog open={report !== null} onClose={onClose} maxWidth="md" fullWidth>
      <DialogTitle>Diagnostic report</DialogTitle>
      <DialogContent>
        <Typography variant="body2" color="text.secondary" sx={{ mb: 1 }}>
          Select and copy the report. It contains no keys, PINs or certificates.
        </Typography>
        <TextField
          value={report ?? ""}
          multiline
          fullWidth
          minRows={12}
          maxRows={24}
          slotProps={{ input: { readOnly: true, sx: { fontFamily: "monospace", fontSize: 12 } } }}
        />
      </DialogContent>
      <DialogActions sx={{ px: 3, pb: 2 }}>
        <Button onClick={onClose}>Close</Button>
      </DialogActions>
    </Dialog>
  );
}

export function DiagnosticsPage() {
  const daemon = useDaemon();
  const { server, notify } = daemon;
  const now = useNow(10_000);
  const [running, setRunning] = useState(false);
  const [report, setReport] = useState<string | null>(null);
  if (!server) return <PageHeader title="Diagnostics" />;
  const listeners = server.listeners;

  const runChecks = async () => {
    setRunning(true);
    try {
      // The daemon announces the new report, which replaces the shown one.
      await api.runHealthCheck();
      notify("success", "Host checks finished.");
    } catch (error) {
      notify(
        "error",
        errorKind(error) === "incompatible" ? "This Pyroshine version cannot re-run its checks; restart it instead." : errorMessage(error),
      );
    } finally {
      setRunning(false);
    }
  };

  const [saving, setSaving] = useState(false);
  const saveBundle = async () => {
    setSaving(true);
    try {
      const bundle = await api.supportBundle();
      download(bundle.blob, bundle.name);
      notify("success", `Support bundle ${bundle.name} downloaded. Identifying values are redacted; review it before sharing.`);
    } catch (error) {
      notify(
        "error",
        errorKind(error) === "incompatible" ? "This Pyroshine version cannot build support bundles; update it." : errorMessage(error),
      );
    } finally {
      setSaving(false);
    }
  };

  const copyReport = async () => {
    let text: string;
    try {
      text = await api.diagnosticReport();
    } catch (error) {
      notify("error", errorMessage(error));
      return;
    }
    try {
      await navigator.clipboard.writeText(text);
      notify("success", "Diagnostic report copied.");
    } catch {
      setReport(text);
    }
  };

  return (
    <>
      <PageHeader
        title="Diagnostics"
        subtitle="What Pyroshine detected on this host, and recent problems with their causes."
        actions={
          <>
            <Button variant="outlined" startIcon={<ContentCopy />} onClick={() => void copyReport()}>
              Copy diagnostics
            </Button>
            <Button variant="outlined" startIcon={<Archive />} disabled={saving} onClick={() => void saveBundle()}>
              {saving ? "Collecting…" : "Save support bundle"}
            </Button>
            <RestartButton />
            <Button variant="contained" startIcon={<Refresh />} disabled={running} onClick={() => void runChecks()}>
              {running ? "Checking…" : "Run checks"}
            </Button>
          </>
        }
      />
      <Stack sx={{ gap: 2 }}>
        <EventsCard events={daemon.events} now={now} />
        <HealthCard health={server.health} running={running} now={now} />

        <Stack direction={{ xs: "column", md: "row" }} sx={{ gap: 2 }}>
          <Card sx={{ flex: 1 }}>
            <CardContent>
              <Typography variant="h6" sx={{ mb: 2 }}>
                Server
              </Typography>
              <Facts
                rows={[
                  ["Host name", server.name],
                  ["Version", server.version],
                  ["Running for", duration(now - server.started_at_ms)],
                  ["Process", String(server.pid)],
                  ["Configuration", server.config_path],
                  ["New pairings", server.pairing_enabled ? "Allowed" : "Disabled"],
                  ["Management API", `version ${server.api_version}`],
                ]}
              />
            </CardContent>
          </Card>
          <Card sx={{ flex: 1 }}>
            <CardContent>
              <Typography variant="h6" sx={{ mb: 2 }}>
                Capabilities
              </Typography>
              <Facts
                rows={[
                  ["GPU", server.capabilities.gpu || "—"],
                  ["DMA-BUF import", server.capabilities.dma_buf ? "Supported" : "Unavailable"],
                  ["HDR", server.capabilities.hdr_advertised ? "Advertised to clients" : "Not advertised"],
                ]}
              />
              <Typography variant="body2" color="text.secondary" sx={{ mt: 2, mb: 1 }}>
                Verified codec profiles (offered to clients until Pyroshine restarts)
              </Typography>
              <Stack direction="row" sx={{ gap: 1, flexWrap: "wrap" }}>
                {server.capabilities.codecs.length === 0 ? (
                  <Typography variant="body2">None</Typography>
                ) : (
                  server.capabilities.codecs.map((codec) => <Chip key={codec.id} label={codec.label} size="small" />)
                )}
              </Stack>
            </CardContent>
          </Card>
        </Stack>

        <Card>
          <CardContent>
            <Typography variant="h6" sx={{ mb: 2 }}>
              Listeners
            </Typography>
            <Facts
              rows={[
                ["Address", listeners.address],
                ["HTTP / HTTPS", `TCP ${listeners.http_port} / ${listeners.https_port}`],
                ["RTSP", `TCP ${listeners.rtsp_port}`],
                ["Video / audio / control", `UDP ${listeners.video_port} / ${listeners.audio_port} / ${listeners.control_port}`],
              ]}
            />
          </CardContent>
        </Card>

        <Card>
          <CardContent>
            <Typography variant="h6" sx={{ mb: 1 }}>
              Management
            </Typography>
            <Typography variant="body2" color="text.secondary">
              This page is served by Pyroshine itself. Closing it never stops Pyroshine or a
              stream. The desktop tray, when installed, only opens these pages and shows notifications; without it,
              open this address in any browser, or pair from a terminal with <code>pyroshine pair</code>.
            </Typography>
          </CardContent>
        </Card>
      </Stack>
      <ReportDialog report={report} onClose={() => setReport(null)} />
    </>
  );
}
