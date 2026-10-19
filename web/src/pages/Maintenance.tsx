import { useCallback, useEffect, useState } from "react";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Chip from "@mui/material/Chip";
import Divider from "@mui/material/Divider";
import Stack from "@mui/material/Stack";
import TextField from "@mui/material/TextField";
import Typography from "@mui/material/Typography";
import Logout from "@mui/icons-material/Logout";
import { api, errorMessage } from "../api/client";
import type { WebSession } from "../api/types";
import { CopyableCommand, Facts, PageHeader } from "../components/common";
import { useAuth } from "../state/auth";
import { useDaemon } from "../state/daemon";
import { relative } from "../util/format";

function PasswordCard() {
  const auth = useAuth();
  const { notify } = useDaemon();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const min = auth.state?.min_password_length ?? 8;
  const valid = current.length > 0 && next.length >= min && next === confirm;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.changePassword(current, next);
      setCurrent("");
      setNext("");
      setConfirm("");
      notify("success", "Password changed. Other browsers were signed out.");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card>
      <CardContent sx={{ p: { xs: 2, sm: 3 } }}>
        <Typography variant="h6" sx={{ mb: 0.5 }}>
          Administrator password
        </Typography>
        <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
          Changing it signs out every other browser. Forgot it? Run <code>pyroshine web setup --reset</code> on the host.
        </Typography>
        <Box
          component="form"
          onSubmit={(event) => {
            event.preventDefault();
            if (valid && !busy) void submit();
          }}
        >
          <Stack sx={{ gap: 2, maxWidth: 420 }}>
            <TextField label="Current password" type="password" autoComplete="current-password" value={current} onChange={(e) => setCurrent(e.target.value)} />
            <TextField
              label="New password"
              type="password"
              autoComplete="new-password"
              value={next}
              helperText={`At least ${min} characters.`}
              onChange={(e) => setNext(e.target.value)}
            />
            <TextField
              label="Repeat the new password"
              type="password"
              autoComplete="new-password"
              value={confirm}
              error={confirm.length > 0 && confirm !== next}
              onChange={(e) => setConfirm(e.target.value)}
            />
            {error && <Alert severity="error">{error}</Alert>}
            <Box>
              <Button type="submit" variant="contained" disabled={!valid || busy}>
                Change password
              </Button>
            </Box>
          </Stack>
        </Box>
      </CardContent>
    </Card>
  );
}

function SessionsCard() {
  const auth = useAuth();
  const { notify } = useDaemon();
  const [sessions, setSessions] = useState<WebSession[] | null>(null);

  const load = useCallback(() => {
    api.webSessions().then(setSessions, (e) => notify("error", errorMessage(e)));
  }, [notify]);
  useEffect(load, [load]);

  const revoke = async (id: string) => {
    try {
      await api.revokeWebSession(id);
      load();
    } catch (e) {
      notify("error", errorMessage(e));
    }
  };

  return (
    <Card>
      <CardContent sx={{ p: { xs: 2, sm: 3 } }}>
        <Stack direction="row" sx={{ justifyContent: "space-between", alignItems: "flex-start", gap: 2, flexWrap: "wrap", mb: 1 }}>
          <Box>
            <Typography variant="h6">Signed-in browsers</Typography>
            <Typography variant="body2" color="text.secondary">
              A browser stays signed in for up to 30 days, or 7 days without use.
            </Typography>
          </Box>
          <Button variant="outlined" startIcon={<Logout />} onClick={() => void auth.logout()}>
            Sign out
          </Button>
        </Stack>
        {sessions === null ? null : (
          <Stack divider={<Divider flexItem />} sx={{ mt: 1 }}>
            {sessions.map((session) => (
              <Stack key={session.id} direction={{ xs: "column", sm: "row" }} sx={{ py: 1.5, gap: 1, alignItems: { sm: "center" } }}>
                <Box sx={{ flex: 1, minWidth: 0 }}>
                  <Typography variant="body2" noWrap title={session.agent}>
                    {session.agent || "Unknown browser"}
                  </Typography>
                  <Typography variant="body2" color="text.secondary">
                    From {session.address} · signed in {relative(session.created_at_ms)} · last used {relative(session.last_used_at_ms)}
                  </Typography>
                </Box>
                {session.current ? (
                  <Chip size="small" color="primary" label="This browser" />
                ) : (
                  <Button size="small" color="error" onClick={() => void revoke(session.id)}>
                    Sign out
                  </Button>
                )}
              </Stack>
            ))}
          </Stack>
        )}
      </CardContent>
    </Card>
  );
}

export function MaintenancePage({ navigate }: { navigate: (to: string) => void }) {
  const { server } = useDaemon();
  return (
    <>
      <PageHeader title="Maintenance" subtitle="The service, its restart, and who can use this page." />
      <Stack sx={{ gap: 3 }}>
        <Card>
          <CardContent sx={{ p: { xs: 2, sm: 3 } }}>
            <Typography variant="h6" sx={{ mb: 2 }}>
              Service
            </Typography>
            {server && (
              <Facts
                rows={[
                  ["Version", server.version],
                  ["Running since", new Date(server.started_at_ms).toLocaleString()],
                  ["Configuration", server.config_path],
                  ["Process", String(server.pid)],
                ]}
              />
            )}
            <Alert severity="info" sx={{ my: 2 }}>
              Saved settings take effect when the service restarts. Restarting ends a running stream; pairings, settings and
              history are kept. This page reconnects by itself afterwards.
            </Alert>
            <Stack sx={{ gap: 1, maxWidth: 560 }}>
              <Typography variant="body2" color="text.secondary">
                Restart (on the host or over SSH):
              </Typography>
              <CopyableCommand command='sudo systemctl restart "pyroshine@$USER"' />
              <Typography variant="body2" color="text.secondary">
                Service log:
              </Typography>
              <CopyableCommand command='journalctl -u "pyroshine@$USER" -e' />
            </Stack>
            <Stack direction="row" sx={{ gap: 1, mt: 2, flexWrap: "wrap" }}>
              <Button variant="outlined" onClick={() => navigate("diagnostics")}>
                Support bundle and host checks
              </Button>
              <Button variant="outlined" onClick={() => navigate("history")}>
                Session history
              </Button>
              <Button variant="outlined" onClick={() => navigate("updates")}>
                Updates
              </Button>
            </Stack>
          </CardContent>
        </Card>
        <PasswordCard />
        <SessionsCard />
      </Stack>
    </>
  );
}
