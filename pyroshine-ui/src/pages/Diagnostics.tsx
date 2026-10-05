import Alert from "@mui/material/Alert";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Chip from "@mui/material/Chip";
import Stack from "@mui/material/Stack";
import Table from "@mui/material/Table";
import TableBody from "@mui/material/TableBody";
import TableCell from "@mui/material/TableCell";
import TableRow from "@mui/material/TableRow";
import Typography from "@mui/material/Typography";
import CheckCircle from "@mui/icons-material/CheckCircle";
import ErrorIcon from "@mui/icons-material/Error";
import Warning from "@mui/icons-material/Warning";
import { Facts, PageHeader } from "../components/common";
import { useDaemon, useNow } from "../state/daemon";
import { duration } from "../util/format";

const outcomeIcons = {
  passed: <CheckCircle color="success" fontSize="small" />,
  warning: <Warning color="warning" fontSize="small" />,
  failed: <ErrorIcon color="error" fontSize="small" />,
};

export function DiagnosticsPage() {
  const { server } = useDaemon();
  const now = useNow(10_000);
  if (!server) return <PageHeader title="Diagnostics" />;
  const listeners = server.listeners;
  return (
    <>
      <PageHeader title="Diagnostics" subtitle="What Pyroshine detected when it started. Restart Pyroshine to re-run the checks." />
      <Stack sx={{ gap: 2 }}>
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
                Verified codec profiles
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
              Startup health check
            </Typography>
            {!server.health ? (
              <Alert severity="info">Skipped: Pyroshine was started with --no-health-check.</Alert>
            ) : (
              <Table size="small" aria-label="Health checks">
                <TableBody>
                  {server.health.checks.map((check, index) => (
                    <TableRow key={`${check.name}-${index}`}>
                      <TableCell sx={{ width: 32, pr: 0 }}>{outcomeIcons[check.outcome]}</TableCell>
                      <TableCell sx={{ fontWeight: 500, whiteSpace: "nowrap" }}>{check.name}</TableCell>
                      <TableCell>{check.message}</TableCell>
                      <TableCell align="right" sx={{ color: "text.secondary", whiteSpace: "nowrap" }}>
                        {check.duration_ms} ms
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardContent>
            <Typography variant="h6" sx={{ mb: 1 }}>
              Desktop app
            </Typography>
            <Typography variant="body2" color="text.secondary">
              This app talks to Pyroshine over your session bus. Closing or quitting it never stops Pyroshine or a stream;
              Pyroshine keeps working headless, with pairing on its local web page.
            </Typography>
          </CardContent>
        </Card>
      </Stack>
    </>
  );
}
