import { memo, useCallback, useDeferredValue, useMemo, useState } from "react";
import Alert from "@mui/material/Alert";
import AlertTitle from "@mui/material/AlertTitle";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Chip from "@mui/material/Chip";
import Divider from "@mui/material/Divider";
import InputAdornment from "@mui/material/InputAdornment";
import LinearProgress from "@mui/material/LinearProgress";
import MenuItem from "@mui/material/MenuItem";
import Stack from "@mui/material/Stack";
import Switch from "@mui/material/Switch";
import TextField from "@mui/material/TextField";
import ToggleButton from "@mui/material/ToggleButton";
import ToggleButtonGroup from "@mui/material/ToggleButtonGroup";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import Refresh from "@mui/icons-material/Refresh";
import Search from "@mui/icons-material/Search";
import ImageOutlined from "@mui/icons-material/ImageOutlined";
import HideImageOutlined from "@mui/icons-material/HideImageOutlined";
import EditOutlined from "@mui/icons-material/EditOutlined";
import { call, errorMessage } from "../api/bridge";
import type { LibraryApplication, LibrarySnapshot, LibrarySource, SourceStatus } from "../api/types";
import { PageHeader } from "../components/common";
import { useDaemon, useNow } from "../state/daemon";
import { useLibrary } from "../state/library";
import { relative } from "../util/format";

/// Rows rendered at once; the rest are a "Show more" away, so a library of
/// thousands of entries never blocks the window.
export const PAGE_SIZE = 100;

export type View = "all" | "offered" | "hidden" | "not_offered";

export interface Filter {
  query: string;
  view: View;
  /// `all`, `manual` or a scanner kind (`steam`).
  source: string;
}

/// Entries matching the search (title, source, ID or launcher ID), view and source.
export function filterApplications(applications: LibraryApplication[], filter: Filter): LibraryApplication[] {
  const query = filter.query.trim().toLowerCase();
  return applications.filter((application) => {
    if (filter.source !== "all" && application.source !== filter.source) return false;
    switch (filter.view) {
      case "offered":
        if (!application.offered) return false;
        break;
      case "hidden":
        if (application.state !== "hidden") return false;
        break;
      case "not_offered":
        if (application.offered) return false;
        break;
    }
    if (!query) return true;
    return (
      application.title.toLowerCase().includes(query) ||
      application.source_label.toLowerCase().includes(query) ||
      String(application.id) === query ||
      (application.native_id?.toLowerCase().includes(query) ?? false)
    );
  });
}

const statusLabels: Record<SourceStatus, [string, "default" | "success" | "warning" | "error"]> = {
  pending: ["Not scanned yet", "default"],
  ok: ["OK", "success"],
  partial: ["Partly unavailable", "warning"],
  failed: ["Failed", "error"],
};

const stateLabels: Record<LibraryApplication["state"], [string, "default" | "warning" | "info"] | null> = {
  available: null,
  stale: ["Offline library", "warning"],
  hidden: ["Hidden", "default"],
  shadowed: ["Replaced by manual entry", "info"],
  duplicate: ["Duplicate", "info"],
};

const artworkLabels: Record<string, string> = {
  steam: "Steam library art",
  steam_custom: "Custom art set in Steam",
  launcher: "Launcher cover art",
  desktop_icon: "Desktop entry icon",
  manual: "Configured box art",
  icon_theme: "Matching icon",
};

function SourceCard({ source, now }: { source: LibrarySource; now: number }) {
  const [label, color] = statusLabels[source.status];
  return (
    <Card>
      <CardContent>
        <Stack direction="row" sx={{ alignItems: "center", gap: 1, flexWrap: "wrap" }}>
          <Typography variant="subtitle1" sx={{ fontWeight: 600, flexGrow: 1 }}>
            {source.label}
          </Typography>
          <Chip size="small" label={label} color={color} variant={color === "default" ? "outlined" : "filled"} />
          <Tooltip
            title={
              source.watched
                ? "Installs and removals are noticed within seconds."
                : "Changes are noticed by the periodic check (file notifications are unavailable for this location)."
            }
          >
            <Chip size="small" variant="outlined" label={source.watched ? "Watching" : "Periodic checks"} />
          </Tooltip>
        </Stack>
        <Typography variant="body2" sx={{ fontFamily: "monospace", color: "text.secondary", mt: 0.5, wordBreak: "break-all" }}>
          {source.root}
        </Typography>
        <Typography variant="body2" color="text.secondary" sx={{ mt: 0.5 }}>
          {source.applications} {source.applications === 1 ? "application" : "applications"}
          {source.stale_applications > 0 && ` (${source.stale_applications} from the last successful scan)`}
          {source.last_scan_ms != null && ` · scanned ${relative(source.last_scan_ms, now)}`}
          {source.duration_ms != null && ` in ${source.duration_ms} ms`}
        </Typography>
        {source.message && (
          <Alert severity={source.status === "failed" ? "error" : "warning"} sx={{ mt: 1.5, overflowWrap: "anywhere" }}>
            {source.message}
            {source.hint && (
              <Typography variant="body2" sx={{ mt: 0.5 }}>
                {source.hint}
              </Typography>
            )}
          </Alert>
        )}
      </CardContent>
    </Card>
  );
}

/// One entry. Rows are memoized and use plain `title` hints rather than
/// tooltip components: a page of them must mount quickly while typing.
const ApplicationRow = memo(function ApplicationRow({
  application,
  busy,
  onToggle,
  onEdit,
}: {
  application: LibraryApplication;
  busy: boolean;
  onToggle: (application: LibraryApplication) => void;
  onEdit: () => void;
}) {
  const state = stateLabels[application.state];
  const art = application.artwork ? artworkLabels[application.artwork] ?? "Box art" : "No box art: clients show a placeholder";
  return (
    <Stack direction="row" sx={{ alignItems: "center", gap: 1.5, px: 2, py: 1.25 }} data-key={application.key}>
      <Box title={art} sx={{ color: application.artwork ? "text.secondary" : "text.disabled", display: "flex" }}>
        {application.artwork ? <ImageOutlined fontSize="small" /> : <HideImageOutlined fontSize="small" />}
      </Box>
      <Box sx={{ flexGrow: 1, minWidth: 0 }}>
        <Typography
          variant="body1"
          sx={{ fontWeight: 500, color: application.offered ? "text.primary" : "text.secondary" }}
          noWrap
        >
          {application.title}
        </Typography>
        <Typography variant="body2" color="text.secondary" noWrap>
          {application.source_label}
          {application.native_id && ` · ${application.native_id}`}
          {application.id !== 0 && ` · ID ${application.id}`}
          {application.state_detail && application.state !== "hidden" && ` · ${application.state_detail}`}
        </Typography>
      </Box>
      {application.manual && <Chip size="small" label="Manual" color="primary" variant="outlined" />}
      {state && <Chip size="small" label={state[0]} color={state[1]} variant="outlined" />}
      {application.manual ? (
        <Button size="small" startIcon={<EditOutlined />} onClick={onEdit} title="Manual applications are edited in Settings">
          Edit
        </Button>
      ) : application.can_hide ? (
        <Switch
          checked={application.state !== "hidden"}
          disabled={busy}
          onChange={() => onToggle(application)}
          title={application.state === "hidden" ? "Show to clients" : "Hide from clients"}
          slotProps={{ input: { "aria-label": `Offer ${application.title} to clients` } }}
        />
      ) : (
        <Box sx={{ width: 58 }} />
      )}
    </Stack>
  );
});

export function LibraryContent({
  snapshot,
  navigate,
  onRescan,
  onToggle,
  busyKeys = new Set(),
  rescanning = false,
}: {
  snapshot: LibrarySnapshot;
  navigate: (to: string) => void;
  onRescan: () => void;
  onToggle: (application: LibraryApplication) => void;
  busyKeys?: Set<string>;
  rescanning?: boolean;
}) {
  const now = useNow(5000);
  const [query, setQuery] = useState("");
  const [view, setView] = useState<View>("all");
  const [source, setSource] = useState("all");
  const [limit, setLimit] = useState(PAGE_SIZE);
  // Filtering thousands of entries must not delay typing.
  const deferredQuery = useDeferredValue(query);
  const matches = useMemo(
    () => filterApplications(snapshot.applications, { query: deferredQuery, view, source }),
    [snapshot.applications, deferredQuery, view, source],
  );
  const offered = snapshot.applications.filter((application) => application.offered).length;
  const hidden = snapshot.applications.filter((application) => application.state === "hidden").length;
  const sources = useMemo(
    () => Array.from(new Map(snapshot.applications.map((a) => [a.source, a.source_label])).entries()),
    [snapshot.applications],
  );
  const edit = useCallback(() => navigate("settings"), [navigate]);
  const scanning = snapshot.scanning || rescanning;

  return (
    <>
      <PageHeader
        title="Library"
        subtitle={
          <>
            {offered} offered to clients{hidden > 0 && ` · ${hidden} hidden`}
            {snapshot.last_refresh_ms != null && ` · updated ${relative(snapshot.last_refresh_ms, now)}`}
          </>
        }
        actions={
          <Button variant="contained" startIcon={<Refresh />} onClick={onRescan} disabled={scanning}>
            {scanning ? "Scanning…" : "Rescan"}
          </Button>
        }
      />
      {scanning && <LinearProgress sx={{ mb: 2 }} aria-label="Scanning game libraries" />}
      {snapshot.notice && (
        <Alert severity="warning" sx={{ mb: 2 }}>
          {snapshot.notice}
        </Alert>
      )}
      {!snapshot.auto_refresh && (
        <Alert severity="info" sx={{ mb: 2 }}>
          Automatic updates are off: after installing or removing games, choose Rescan. Turn them on in Settings ›
          Application discovery.
        </Alert>
      )}

      {snapshot.sources.length > 0 ? (
        <Box sx={{ display: "grid", gap: 2, gridTemplateColumns: { xs: "1fr", md: "1fr 1fr" }, mb: 3 }}>
          {snapshot.sources.map((source) => (
            <SourceCard key={source.scanner} source={source} now={now} />
          ))}
        </Box>
      ) : (
        <Alert severity="info" sx={{ mb: 3 }}>
          <AlertTitle>No launchers are scanned</AlertTitle>
          Add Steam, Lutris, Heroic or desktop-entry scanners in Settings › Application discovery to offer installed
          games automatically.
        </Alert>
      )}

      <Card>
        <Stack direction="row" sx={{ gap: 2, p: 2, flexWrap: "wrap", alignItems: "center" }}>
          <TextField
            size="small"
            placeholder="Search by title, launcher or ID"
            value={query}
            onChange={(event) => {
              setQuery(event.target.value);
              setLimit(PAGE_SIZE);
            }}
            sx={{ flexGrow: 1, minWidth: 240 }}
            slotProps={{
              input: {
                startAdornment: (
                  <InputAdornment position="start">
                    <Search fontSize="small" />
                  </InputAdornment>
                ),
              },
              htmlInput: { "aria-label": "Search applications" },
            }}
          />
          <TextField
            select
            size="small"
            value={source}
            onChange={(event) => setSource(event.target.value)}
            sx={{ minWidth: 160 }}
            slotProps={{ htmlInput: { "aria-label": "Source" } }}
          >
            <MenuItem value="all">All sources</MenuItem>
            {sources.map(([id, label]) => (
              <MenuItem key={id} value={id}>
                {label}
              </MenuItem>
            ))}
          </TextField>
          <ToggleButtonGroup
            size="small"
            exclusive
            value={view}
            onChange={(_, next: View | null) => next && setView(next)}
            aria-label="Show"
          >
            <ToggleButton value="all">All</ToggleButton>
            <ToggleButton value="offered">Offered</ToggleButton>
            <ToggleButton value="hidden">Hidden</ToggleButton>
            <ToggleButton value="not_offered">Not offered</ToggleButton>
          </ToggleButtonGroup>
        </Stack>
        <Divider />
        {matches.length === 0 ? (
          <Typography color="text.secondary" sx={{ p: 3, textAlign: "center" }}>
            {snapshot.applications.length === 0 ? "No applications yet." : "No application matches."}
          </Typography>
        ) : (
          <Box role="list" aria-label="Applications">
            {matches.slice(0, limit).map((application, index) => (
              <Box role="listitem" key={application.key}>
                {index > 0 && <Divider />}
                <ApplicationRow
                  application={application}
                  busy={busyKeys.has(application.key)}
                  onToggle={onToggle}
                  onEdit={edit}
                />
              </Box>
            ))}
          </Box>
        )}
        {matches.length > limit && (
          <>
            <Divider />
            <Stack direction="row" sx={{ p: 1.5, justifyContent: "center", alignItems: "center", gap: 2 }}>
              <Typography variant="body2" color="text.secondary">
                Showing {limit} of {matches.length}
              </Typography>
              <Button size="small" onClick={() => setLimit((current) => current + PAGE_SIZE)}>
                Show more
              </Button>
            </Stack>
          </>
        )}
      </Card>
    </>
  );
}

export function LibraryPage({ navigate }: { navigate: (to: string) => void }) {
  const daemon = useDaemon();
  const { snapshot, error, reload } = useLibrary(daemon.connected);
  const [busyKeys, setBusyKeys] = useState<Set<string>>(new Set());
  const [rescanning, setRescanning] = useState(false);
  const { notify } = daemon;

  const onToggle = useCallback(
    (application: LibraryApplication) => {
      setBusyKeys((current) => new Set(current).add(application.key));
      call("set_application_hidden", { key: application.key, hidden: application.state !== "hidden" })
        .then(reload)
        .catch((e: unknown) => notify("error", errorMessage(e)))
        .finally(() =>
          setBusyKeys((current) => {
            const next = new Set(current);
            next.delete(application.key);
            return next;
          }),
        );
    },
    [notify, reload],
  );

  if (error) {
    return (
      <>
        <PageHeader title="Library" />
        <Alert
          severity={error.kind === "incompatible" ? "info" : "error"}
          action={
            error.kind === "incompatible" ? undefined : (
              <Button color="inherit" size="small" onClick={() => void reload()}>
                Retry
              </Button>
            )
          }
        >
          {error.kind === "incompatible"
            ? "This Pyroshine server does not report its game library. Update Pyroshine to manage discovered games here."
            : error.message}
        </Alert>
      </>
    );
  }
  if (!snapshot) {
    return (
      <>
        <PageHeader title="Library" />
        <LinearProgress aria-label="Loading the library" />
      </>
    );
  }

  const rescan = async () => {
    setRescanning(true);
    try {
      await call("rescan_library");
    } catch (e) {
      notify("error", errorMessage(e));
    } finally {
      // The daemon's `scanning` flag takes over once the scan starts.
      setTimeout(() => setRescanning(false), 500);
    }
  };

  return (
    <LibraryContent
      snapshot={snapshot}
      navigate={navigate}
      onRescan={() => void rescan()}
      onToggle={onToggle}
      busyKeys={busyKeys}
      rescanning={rescanning}
    />
  );
}
