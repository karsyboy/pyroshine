import { useEffect, useRef, useState } from "react";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Chip from "@mui/material/Chip";
import Divider from "@mui/material/Divider";
import IconButton from "@mui/material/IconButton";
import InputAdornment from "@mui/material/InputAdornment";
import LinearProgress from "@mui/material/LinearProgress";
import Stack from "@mui/material/Stack";
import TextField from "@mui/material/TextField";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import Check from "@mui/icons-material/Check";
import Close from "@mui/icons-material/Close";
import DeleteOutline from "@mui/icons-material/DeleteOutlineOutlined";
import EditOutlined from "@mui/icons-material/EditOutlined";
import Fingerprint from "@mui/icons-material/Fingerprint";
import Devices from "@mui/icons-material/Devices";
import PhonelinkLock from "@mui/icons-material/PhonelinkLock";
import { call, errorKind, errorMessage } from "../api/bridge";
import type { PairedClient, PendingPairing } from "../api/types";
import { ConfirmDialog, PageHeader } from "../components/common";
import { useDaemon, useNow } from "../state/daemon";
import { fingerprint, relative } from "../util/format";

/// Remember when each request's approval window closes, from the first
/// snapshot that reported it.
function useDeadlines(requests: PendingPairing[]): Map<string, number> {
  const deadlines = useRef(new Map<string, number>());
  for (const request of requests) {
    if (!deadlines.current.has(request.request)) {
      deadlines.current.set(request.request, Date.now() + request.approval_expires_in_ms);
    }
  }
  return deadlines.current;
}

/// One pending request. Its form state lives in this component, keyed by the
/// request token: a request that replaces it is a new card with an empty form,
/// so a PIN typed for one request can never be submitted for another.
function PendingCard({ request, deadline, focused }: { request: PendingPairing; deadline: number; focused: boolean }) {
  const daemon = useDaemon();
  const now = useNow();
  const [pin, setPin] = useState("");
  const [label, setLabel] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const remaining = Math.max(0, deadline - now);
  const expired = remaining === 0 && !request.approved;
  const pinValid = /^\d{4,16}$/.test(pin);

  const approve = async () => {
    setBusy(true);
    setError(null);
    try {
      await call("approve_pairing", { clientId: request.client_id, request: request.request, pin, label });
    } catch (e) {
      setError(errorMessage(e));
      if (errorKind(e) === "not_found") daemon.notify("warning", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const reject = async () => {
    setBusy(true);
    try {
      await call("reject_pairing", { clientId: request.client_id, request: request.request });
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card sx={{ borderColor: focused ? "primary.main" : undefined, borderWidth: focused ? 2 : 1 }}>
      {request.approved && <LinearProgress />}
      <CardContent sx={{ p: 3 }}>
        <Stack direction={{ xs: "column", md: "row" }} sx={{ gap: 3 }}>
          <Stack sx={{ gap: 1, flex: 1, minWidth: 0 }}>
            <Stack direction="row" sx={{ alignItems: "center", gap: 1 }}>
              <PhonelinkLock color="primary" />
              <Typography variant="h6">Pairing request from {request.requester}</Typography>
            </Stack>
            <Stack direction="row" sx={{ alignItems: "center", gap: 1, color: "text.secondary" }}>
              <Fingerprint fontSize="small" />
              <Tooltip title={request.fingerprint ?? ""}>
                <Typography variant="body2" sx={{ fontFamily: "monospace" }}>
                  {fingerprint(request.fingerprint)}
                </Typography>
              </Tooltip>
            </Stack>
            <Typography variant="body2" color="text.secondary">
              Client ID {request.client_id} · received {relative(request.received_at_ms, now)}
            </Typography>
            <Typography variant="body2" color="text.secondary">
              Moonlight does not send a device name. Check the requester address before approving, and only enter a PIN
              that a client you are pairing right now shows.
            </Typography>
          </Stack>

          <Stack sx={{ gap: 1.5, width: { md: 340 } }}>
            {request.approved ? (
              <Alert severity="info">PIN entered. Waiting for Moonlight to finish pairing…</Alert>
            ) : expired ? (
              <Alert severity="warning">This request expired. Start pairing again in Moonlight.</Alert>
            ) : (
              <>
                <TextField
                  label="PIN shown in Moonlight"
                  value={pin}
                  autoFocus={focused}
                  onChange={(event) => setPin(event.target.value.replace(/\D/g, "").slice(0, 16))}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" && pinValid && !busy) void approve();
                  }}
                  slotProps={{
                    htmlInput: { inputMode: "numeric", autoComplete: "off", "aria-describedby": `expiry-${request.request}` },
                  }}
                  sx={{ "& input": { fontSize: 22, letterSpacing: 6, fontFamily: "monospace" } }}
                />
                <TextField
                  label="Name this client (optional)"
                  value={label}
                  onChange={(event) => setLabel(event.target.value.slice(0, 64))}
                  placeholder="Living room TV"
                />
                <Stack direction="row" sx={{ gap: 1 }}>
                  <Button variant="contained" startIcon={<Check />} disabled={!pinValid || busy} onClick={() => void approve()} sx={{ flex: 1 }}>
                    Pair
                  </Button>
                  <Button variant="outlined" color="inherit" startIcon={<Close />} disabled={busy} onClick={() => void reject()}>
                    Reject
                  </Button>
                </Stack>
                <Typography id={`expiry-${request.request}`} variant="caption" color="text.secondary">
                  Expires in {Math.floor(remaining / 60000)}:{String(Math.floor((remaining % 60000) / 1000)).padStart(2, "0")}
                </Typography>
              </>
            )}
            {error && <Alert severity="error">{error}</Alert>}
          </Stack>
        </Stack>
      </CardContent>
    </Card>
  );
}

function ClientRow({ client }: { client: PairedClient }) {
  const daemon = useDaemon();
  const now = useNow(15000);
  const [editing, setEditing] = useState(false);
  const [label, setLabel] = useState(client.label ?? "");
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const streaming = daemon.session?.phase !== "idle" && daemon.session?.phase !== undefined;

  const rename = async () => {
    try {
      await call("rename_client", { fingerprint: client.fingerprint, label });
      setEditing(false);
    } catch (e) {
      daemon.notify("error", errorMessage(e));
    }
  };
  const revoke = async () => {
    setBusy(true);
    try {
      await call("revoke_client", { fingerprint: client.fingerprint });
      daemon.notify("success", `${client.label ?? "Client"} can no longer connect.`);
      setConfirm(false);
    } catch (e) {
      daemon.notify("error", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Box sx={{ py: 2, px: 3 }}>
      <Stack direction="row" sx={{ alignItems: "center", gap: 2 }}>
        <Devices color="action" />
        <Box sx={{ flex: 1, minWidth: 0 }}>
          {editing ? (
            <Stack direction="row" sx={{ gap: 1, alignItems: "center", maxWidth: 420 }}>
              <TextField
                value={label}
                autoFocus
                placeholder="Name"
                onChange={(event) => setLabel(event.target.value.slice(0, 64))}
                onKeyDown={(event) => {
                  if (event.key === "Enter") void rename();
                  if (event.key === "Escape") setEditing(false);
                }}
                slotProps={{
                  input: {
                    endAdornment: (
                      <InputAdornment position="end">
                        <IconButton size="small" aria-label="Save name" onClick={() => void rename()}>
                          <Check fontSize="small" />
                        </IconButton>
                      </InputAdornment>
                    ),
                  },
                }}
              />
            </Stack>
          ) : (
            <Stack direction="row" sx={{ alignItems: "center", gap: 0.5 }}>
              <Typography variant="subtitle1" sx={{ fontWeight: 500 }}>
                {client.label ?? "Unnamed client"}
              </Typography>
              <Tooltip title="Rename">
                <IconButton size="small" aria-label="Rename client" onClick={() => setEditing(true)}>
                  <EditOutlined fontSize="small" />
                </IconButton>
              </Tooltip>
            </Stack>
          )}
          <Tooltip title={client.fingerprint}>
            <Typography variant="body2" color="text.secondary" sx={{ fontFamily: "monospace" }}>
              {fingerprint(client.fingerprint)}
            </Typography>
          </Tooltip>
          <Typography variant="caption" color="text.secondary">
            {client.paired_at_ms ? `Paired ${relative(client.paired_at_ms, now)} · ` : ""}
            {client.last_seen_ms
              ? `Last seen ${relative(client.last_seen_ms, now)} from ${client.last_address}`
              : "Not seen since Pyroshine started"}
          </Typography>
        </Box>
        <Button color="error" variant="text" startIcon={<DeleteOutline />} onClick={() => setConfirm(true)}>
          Revoke
        </Button>
      </Stack>
      <ConfirmDialog
        open={confirm}
        title="Revoke this client?"
        confirm={busy ? "Revoking…" : "Revoke"}
        destructive
        busy={busy}
        onConfirm={() => void revoke()}
        onClose={() => setConfirm(false)}
      >
        {client.label ?? "This client"} will need to pair again before it can connect.
        {streaming && " Revoking also ends the current session, because sessions do not record which client started them."}
      </ConfirmDialog>
    </Box>
  );
}

export function ClientsPage({ request }: { request: string | null }) {
  const daemon = useDaemon();
  const requests = daemon.pairing?.requests ?? [];
  const deadlines = useDeadlines(requests);
  const clients = daemon.clients?.clients ?? [];
  // Warn only about links to requests this page never saw (expired before it
  // opened); a request that completes while shown is reported by its outcome.
  const seen = useRef(new Set<string>());
  requests.forEach((pending) => seen.current.add(pending.request));
  const missing = request && !seen.current.has(request);
  const [missingDismissed, setMissingDismissed] = useState(false);
  useEffect(() => setMissingDismissed(false), [request]);
  const port = daemon.server?.listeners.http_port ?? 47989;

  return (
    <>
      <PageHeader title="Clients" subtitle="Approve pairing requests and manage the devices that can stream from this host." />

      <Typography variant="h6" sx={{ mb: 1.5 }}>
        Pairing requests
      </Typography>
      {daemon.pairing && !daemon.pairing.enabled && (
        <Alert severity="info" sx={{ mb: 2 }}>
          New pairings are disabled (Settings → Pairing &amp; security → Allow new pairings).
        </Alert>
      )}
      {missing && !missingDismissed && (
        <Alert severity="warning" sx={{ mb: 2 }} onClose={() => setMissingDismissed(true)}>
          That pairing request is no longer waiting. It may have expired, been completed, or been replaced by a newer
          request from the same client.
        </Alert>
      )}
      <Stack sx={{ gap: 2, mb: 4 }}>
        {requests.length === 0 ? (
          <Card>
            <CardContent sx={{ p: 3 }}>
              <Typography sx={{ mb: 1 }}>No client is waiting to pair.</Typography>
              <Typography variant="body2" color="text.secondary">
                In Moonlight, add this host (it is discovered automatically on the local network) and select it. Moonlight
                shows a PIN and the request appears here — this page opens from the notification.
              </Typography>
            </CardContent>
          </Card>
        ) : (
          requests.map((pending) => (
            <PendingCard
              key={pending.request}
              request={pending}
              deadline={deadlines.get(pending.request) ?? Date.now()}
              focused={pending.request === request || requests.length === 1}
            />
          ))
        )}
      </Stack>

      <Stack direction="row" sx={{ alignItems: "baseline", gap: 1, mb: 1.5 }}>
        <Typography variant="h6">Paired clients</Typography>
        <Chip size="small" label={clients.length} />
      </Stack>
      <Card sx={{ mb: 3 }}>
        {clients.length === 0 ? (
          <CardContent sx={{ p: 3 }}>
            <Typography color="text.secondary">No paired clients.</Typography>
          </CardContent>
        ) : (
          clients.map((client, index) => (
            <Box key={client.fingerprint}>
              {index > 0 && <Divider />}
              <ClientRow client={client} />
            </Box>
          ))
        )}
      </Card>
      {daemon.clients && daemon.clients.legacy_client_ids.length > 0 && (
        <Alert severity="info" sx={{ mb: 3 }}>
          The pairing state also lists {daemon.clients.legacy_client_ids.length} older client IDs without a known
          certificate. They cannot authorize connections on their own.
        </Alert>
      )}
      <Typography variant="body2" color="text.secondary">
        Without this app (for example on a headless host), approve pairing on the host-local page linked in the
        Pyroshine log (<Box component="code">http://localhost:{port}/pin?uniqueid=…</Box>), forwarding the port over SSH
        if needed.
      </Typography>
    </>
  );
}
