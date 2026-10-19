import { useEffect, useState } from "react";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import CircularProgress from "@mui/material/CircularProgress";
import FormControlLabel from "@mui/material/FormControlLabel";
import Link from "@mui/material/Link";
import MenuItem from "@mui/material/MenuItem";
import Stack from "@mui/material/Stack";
import Switch from "@mui/material/Switch";
import TextField from "@mui/material/TextField";
import Typography from "@mui/material/Typography";
import NewReleasesOutlined from "@mui/icons-material/NewReleasesOutlined";
import Refresh from "@mui/icons-material/Refresh";
import { api, errorMessage } from "../api/client";
import type { UpdateStatus } from "../api/types";
import { CopyableCommand, Facts, PageHeader } from "../components/common";
import { useDaemon } from "../state/daemon";
import { relative } from "../util/format";

/// A step that contains a shell command is shown with a copy button.
function Step({ text }: { text: string }) {
  const [before, command] = text.split(/:\s(?=sudo |nix |pacman )/);
  return (
    <Box>
      <Typography variant="body2">{command ? `${before}:` : text}</Typography>
      {command && (
        <Box sx={{ mt: 0.5, maxWidth: 560 }}>
          <CopyableCommand command={command} />
        </Box>
      )}
    </Box>
  );
}

export function UpdatesPage() {
  const { update, notify } = useDaemon();
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.updates().then(setStatus, (e) => setError(errorMessage(e)));
  }, []);
  // An automatic check reports its result over the event stream.
  useEffect(() => {
    if (update) setStatus(update);
  }, [update]);

  const change = async (automatic: boolean, channel: UpdateStatus["channel"]) => {
    try {
      setStatus(await api.updateSettings(automatic, channel));
    } catch (e) {
      notify("error", errorMessage(e));
    }
  };

  const check = async () => {
    setBusy(true);
    try {
      const result = await api.checkUpdates();
      setStatus(result);
      if (!result.last_error && !result.update_available) notify("success", "Pyroshine is up to date.");
    } catch (e) {
      notify("error", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const dismiss = async (version: string) => {
    try {
      setStatus(await api.dismissUpdate(version));
    } catch (e) {
      notify("error", errorMessage(e));
    }
  };

  return (
    <>
      <PageHeader
        title="Updates"
        subtitle="New Pyroshine releases. Pyroshine never downloads or installs anything itself."
        actions={
          <Button
            variant="outlined"
            startIcon={busy || status?.checking ? <CircularProgress size={16} /> : <Refresh />}
            disabled={busy || !status}
            onClick={() => void check()}
          >
            Check now
          </Button>
        }
      />
      {error && <Alert severity="error">{error}</Alert>}
      {status && (
        <Stack sx={{ gap: 3 }}>
          {status.update_available && status.latest && (
            <Card sx={{ borderColor: status.dismissed ? undefined : "primary.main" }}>
              <CardContent sx={{ p: { xs: 2, sm: 3 } }}>
                <Stack sx={{ gap: 1.5 }}>
                  <Stack direction="row" sx={{ gap: 1, alignItems: "center" }}>
                    <NewReleasesOutlined color="primary" />
                    <Typography variant="h6">
                      Pyroshine {status.latest.version} is available{status.latest.prerelease ? " (prerelease)" : ""}
                    </Typography>
                  </Stack>
                  <Typography variant="body2" color="text.secondary">
                    You run {status.current_version}.{" "}
                    <Link href={status.latest.url} target="_blank" rel="noopener noreferrer">
                      Release notes and downloads
                    </Link>{" "}
                    (verify downloads against the release's SHA256SUMS).
                  </Typography>
                  <Typography variant="subtitle2" sx={{ mt: 1 }}>
                    Update this installation ({status.install.label})
                  </Typography>
                  <Stack sx={{ gap: 1.5 }}>
                    {status.install.steps.map((step) => (
                      <Step key={step} text={step} />
                    ))}
                  </Stack>
                  <Typography variant="body2" color="text.secondary">
                    Restarting the service ends a running stream. Configuration, pairings and history are kept.
                  </Typography>
                  {!status.dismissed && (
                    <Box>
                      <Button size="small" onClick={() => void dismiss(status.latest!.version)}>
                        Don't remind me about {status.latest.version}
                      </Button>
                    </Box>
                  )}
                </Stack>
              </CardContent>
            </Card>
          )}
          {!status.update_available && status.latest && !status.last_error && (
            <Alert severity="success">Pyroshine {status.current_version} is the newest {status.channel} release.</Alert>
          )}
          {status.last_error && <Alert severity="warning">The last check failed: {status.last_error}</Alert>}

          <Card>
            <CardContent sx={{ p: { xs: 2, sm: 3 } }}>
              <Stack sx={{ gap: 2 }}>
                <Facts
                  rows={[
                    ["Installed version", status.current_version],
                    ["Installation", status.install.label],
                    ["Last check", status.last_checked_ms ? relative(status.last_checked_ms) : "Not checked yet"],
                  ]}
                />
                <FormControlLabel
                  control={<Switch checked={status.automatic} onChange={(event) => void change(event.target.checked, status.channel)} />}
                  label="Check for new releases once a day"
                />
                <Typography variant="body2" color="text.secondary" sx={{ mt: -1.5 }}>
                  Asks GitHub for the release list over HTTPS; nothing about this host is sent beyond the request itself.
                </Typography>
                <TextField
                  select
                  label="Release channel"
                  value={status.channel}
                  onChange={(event) => void change(status.automatic, event.target.value as UpdateStatus["channel"])}
                  sx={{ maxWidth: 320 }}
                >
                  <MenuItem value="stable">Stable releases</MenuItem>
                  <MenuItem value="prerelease">Stable and prereleases</MenuItem>
                </TextField>
              </Stack>
            </CardContent>
          </Card>
        </Stack>
      )}
    </>
  );
}
