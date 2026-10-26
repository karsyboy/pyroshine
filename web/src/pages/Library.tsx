import { memo, useCallback, useDeferredValue, useMemo, useState, type ReactNode } from "react";
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
import ListItemText from "@mui/material/ListItemText";
import Menu from "@mui/material/Menu";
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
import HideImageOutlined from "@mui/icons-material/HideImageOutlined";
import EditOutlined from "@mui/icons-material/EditOutlined";
import Add from "@mui/icons-material/Add";
import RestartAlt from "@mui/icons-material/RestartAlt";
import Tune from "@mui/icons-material/Tune";
import { api, artworkUrl, errorMessage } from "../api/client";
import type { LibraryApplication, LibrarySnapshot, LibrarySource, ScannerVariant, SourceStatus } from "../api/types";
import { ConfirmDialog, PageHeader } from "../components/common";
import { ItemDialog, newScanner } from "../components/lists";
import { RestartButton } from "../components/Restart";
import { configIndex, scannerIndex, useLibraryConfig, type Item } from "../state/applications";
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

function SourceCard({ source, now, onEdit }: { source: LibrarySource; now: number; onEdit?: (source: LibrarySource) => void }) {
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
          {onEdit && (
            <Button size="small" startIcon={<EditOutlined />} onClick={() => onEdit(source)}>
              Edit
            </Button>
          )}
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

/// Box art as clients see it, scaled down; a placeholder without art.
function Artwork({ application, version }: { application: LibraryApplication; version: number }) {
  const [failed, setFailed] = useState(false);
  const label = application.artwork ? artworkLabels[application.artwork] ?? "Box art" : "No box art: clients show a placeholder";
  const frame = { width: 40, height: 53, borderRadius: 1, flexShrink: 0, bgcolor: "action.hover" } as const;
  if (!application.artwork || failed) {
    return (
      <Box title={label} sx={{ ...frame, display: "flex", alignItems: "center", justifyContent: "center", color: "text.disabled" }}>
        <HideImageOutlined fontSize="small" />
      </Box>
    );
  }
  return (
    <Box
      component="img"
      src={artworkUrl(application.key, version)}
      alt=""
      title={label}
      loading="lazy"
      decoding="async"
      onError={() => setFailed(true)}
      sx={{ ...frame, objectFit: "cover", display: "block" }}
    />
  );
}

/// One entry. Rows are memoized and use plain `title` hints rather than
/// tooltip components: a page of them must mount quickly while typing.
const ApplicationRow = memo(function ApplicationRow({
  application,
  version,
  busy,
  onToggle,
  onEdit,
}: {
  application: LibraryApplication;
  version: number;
  busy: boolean;
  onToggle: (application: LibraryApplication) => void;
  onEdit?: (application: LibraryApplication) => void;
}) {
  const state = stateLabels[application.state];
  return (
    <Stack direction="row" sx={{ alignItems: "center", gap: 1.5, px: 2, py: 1 }} data-key={application.key}>
      <Artwork application={application} version={version} />
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
        <Button size="small" startIcon={<EditOutlined />} disabled={!onEdit} onClick={() => onEdit?.(application)}>
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
  onRescan,
  onToggle,
  onAdd,
  onEdit,
  onEditSource,
  scannerVariants = [],
  onAddScanner,
  onOptions,
  notice,
  busyKeys = new Set(),
  rescanning = false,
}: {
  snapshot: LibrarySnapshot;
  navigate?: (to: string) => void;
  onRescan: () => void;
  onToggle: (application: LibraryApplication) => void;
  /// Add a static application; absent while the configuration cannot be edited.
  onAdd?: () => void;
  onEdit?: (application: LibraryApplication) => void;
  /// Edit a launcher's scanner; absent while the configuration cannot be edited.
  onEditSource?: (source: LibrarySource) => void;
  scannerVariants?: ScannerVariant[];
  onAddScanner?: (type: string) => void;
  /// Discovery options (automatic updates).
  onOptions?: () => void;
  /// Shown under the header (restart and read-only notices).
  notice?: ReactNode;
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
  const scanning = snapshot.scanning || rescanning;
  const [scannerMenu, setScannerMenu] = useState<HTMLElement | null>(null);

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
          <>
            <Button variant="outlined" startIcon={<Add />} onClick={onAdd} disabled={!onAdd}>
              Add application
            </Button>
            <Button variant="contained" startIcon={<Refresh />} onClick={onRescan} disabled={scanning}>
              {scanning ? "Scanning…" : "Rescan"}
            </Button>
          </>
        }
      />
      {notice}
      {scanning && <LinearProgress sx={{ mb: 2 }} aria-label="Scanning game libraries" />}
      {snapshot.notice && (
        <Alert severity="warning" sx={{ mb: 2 }}>
          {snapshot.notice}
        </Alert>
      )}
      {!snapshot.auto_refresh && (
        <Alert severity="info" sx={{ mb: 2 }}>
          Automatic updates are off: after installing or removing games, choose Rescan, or turn them on in Discovery
          options.
        </Alert>
      )}

      <Stack direction="row" sx={{ alignItems: "center", gap: 1, mb: 1.5, flexWrap: "wrap" }}>
        <Typography variant="h6" sx={{ flexGrow: 1 }}>
          Game launchers
        </Typography>
        <Button startIcon={<Tune />} onClick={onOptions} disabled={!onOptions}>
          Discovery options
        </Button>
        <Button startIcon={<Add />} onClick={(event) => setScannerMenu(event.currentTarget)} disabled={!onAddScanner}>
          Add launcher
        </Button>
      </Stack>
      <Menu anchorEl={scannerMenu} open={scannerMenu !== null} onClose={() => setScannerMenu(null)}>
        {scannerVariants.map((variant) => (
          <MenuItem
            key={variant.id}
            onClick={() => {
              setScannerMenu(null);
              onAddScanner?.(variant.id);
            }}
          >
            <ListItemText primary={variant.label} secondary={variant.description} />
          </MenuItem>
        ))}
      </Menu>
      {snapshot.sources.length > 0 ? (
        <Box sx={{ display: "grid", gap: 2, gridTemplateColumns: { xs: "1fr", md: "1fr 1fr" }, mb: 3 }}>
          {snapshot.sources.map((source) => (
            <SourceCard key={source.scanner} source={source} now={now} onEdit={onEditSource} />
          ))}
        </Box>
      ) : (
        <Alert severity="info" sx={{ mb: 3 }}>
          <AlertTitle>No launchers are scanned</AlertTitle>
          Add Steam, Lutris, Heroic, Bottles or desktop entries with Add launcher to offer installed games automatically.
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
                  version={snapshot.generation}
                  busy={busyKeys.has(application.key)}
                  onToggle={onToggle}
                  onEdit={onEdit}
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

/// Starting values of a new static application.
const NEW_APPLICATION: Item = { title: "", command: [""], launch_timeout_secs: 2 };

/// What the dialog edits: a static application, a launcher's scanner or the
/// discovery options; `index` is null for a new entry.
interface Editing {
  what: "application" | "scanner" | "options";
  index: number | null;
  item: Item;
}

export function LibraryPage(_props: { navigate: (to: string) => void }) {
  const daemon = useDaemon();
  const { snapshot, error, reload } = useLibrary(daemon.connected);
  const [busyKeys, setBusyKeys] = useState<Set<string>>(new Set());
  const [rescanning, setRescanning] = useState(false);
  const { notify } = daemon;
  const config = useLibraryConfig();
  const [editing, setEditing] = useState<Editing | null>(null);
  const [removing, setRemoving] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  const editable = config.ready && config.writable;
  const open = useCallback((next: Editing) => {
    setSaveError(null);
    setEditing(next);
  }, []);
  const { applications, scanners, discoveryFields, value } = config;
  const onAdd = useCallback(() => open({ what: "application", index: null, item: structuredClone(NEW_APPLICATION) }), [open]);
  const onEdit = useCallback(
    (application: LibraryApplication) => {
      const index = configIndex(application, applications);
      if (index === null) {
        notify("warning", `${application.title} is no longer in the configuration file; it was changed or removed there.`);
        return;
      }
      open({ what: "application", index, item: structuredClone(applications[index]) });
    },
    [applications, notify, open],
  );
  const onAddScanner = useCallback((type: string) => open({ what: "scanner", index: null, item: newScanner(type) }), [open]);
  const onEditSource = useCallback(
    (source: LibrarySource) => {
      const index = scannerIndex(source, scanners);
      if (index === null) {
        notify("warning", `The ${source.label} scanner changed in the configuration file; restart Pyroshine to edit it here.`);
        return;
      }
      open({ what: "scanner", index, item: structuredClone(scanners[index]) });
    },
    [scanners, notify, open],
  );
  const onOptions = useCallback(() => {
    const item: Item = {};
    for (const field of discoveryFields) {
      const current = value(field.path);
      if (current !== undefined) item[field.path] = current;
    }
    open({ what: "options", index: null, item });
  }, [discoveryFields, value, open]);

  const variant = (item: Item) => config.scannerVariants.find((candidate) => candidate.id === item.type);
  const fields = (target: Editing) =>
    target.what === "application"
      ? config.applicationFields
      : target.what === "scanner"
        ? variant(target.item)?.fields ?? []
        : discoveryFields;
  const name = (target: Editing) =>
    target.what === "application"
      ? String(target.item.title || "application")
      : target.what === "scanner"
        ? `${variant(target.item)?.label ?? String(target.item.type)} launcher`
        : "discovery options";

  const replace = (items: Item[], index: number | null, item: Item | null) =>
    index === null
      ? item
        ? [...items, item]
        : items
      : item
        ? items.map((existing, position) => (position === index ? item : existing))
        : items.filter((_, position) => position !== index);

  /// Save `item` (null removes the entry) for `target`.
  const store = async (target: Editing, item: Item | null) => {
    setSaving(true);
    setSaveError(null);
    try {
      if (target.what === "application") await config.saveApplications(replace(applications, target.index, item));
      else if (target.what === "scanner")
        await config.saveScanners(replace(scanners, target.index, item && { ...item, type: target.item.type }));
      else await config.save(Object.fromEntries(discoveryFields.map((field) => [field.path, item?.[field.path]])));
      setEditing(null);
      setRemoving(false);
      const done = item === null ? "Removed." : target.index === null && target.what !== "options" ? "Added." : "Saved.";
      notify("success", `${done} Restart Pyroshine to apply the change.`);
    } catch (failure) {
      setRemoving(false);
      setSaveError(errorMessage(failure));
    } finally {
      setSaving(false);
    }
  };

  const onToggle = useCallback(
    (application: LibraryApplication) => {
      setBusyKeys((current) => new Set(current).add(application.key));
      api.setApplicationHidden(application.key, application.state !== "hidden")
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
      await api.rescanLibrary();
    } catch (e) {
      notify("error", errorMessage(e));
    } finally {
      // The daemon's `scanning` flag takes over once the scan starts.
      setTimeout(() => setRescanning(false), 500);
    }
  };

  const notice = (
    <>
      {config.restartRequired && (
        <Alert
          severity="info"
          icon={<RestartAlt />}
          sx={{ mb: 2 }}
          action={<RestartButton variant="text" label="Restart now" />}
        >
          Saved changes apply when Pyroshine restarts. Restarting ends any active session.
        </Alert>
      )}
      {config.ready && !config.writable && (
        <Alert severity="warning" sx={{ mb: 2 }}>
          Applications and launchers cannot be changed here:{" "}
          {config.readOnlyReason ?? "the configuration file is read-only."}
        </Alert>
      )}
    </>
  );

  return (
    <>
      <LibraryContent
        snapshot={snapshot}
        onRescan={() => void rescan()}
        onToggle={onToggle}
        onAdd={editable ? onAdd : undefined}
        onEdit={editable ? onEdit : undefined}
        onEditSource={editable ? onEditSource : undefined}
        scannerVariants={config.scannerVariants}
        onAddScanner={editable ? onAddScanner : undefined}
        onOptions={editable ? onOptions : undefined}
        notice={notice}
        busyKeys={busyKeys}
        rescanning={rescanning}
      />
      {editing && (
        <ItemDialog
          title={
            editing.what === "options"
              ? "Discovery options"
              : `${editing.index === null ? "Add" : "Edit"} ${name(editing)}`
          }
          fields={fields(editing)}
          initial={editing.item}
          busy={saving}
          error={saveError}
          saveLabel="Save"
          onClose={() => setEditing(null)}
          onRemove={editing.index === null ? undefined : () => setRemoving(true)}
          onSave={(item) => void store(editing, item)}
        />
      )}
      <ConfirmDialog
        open={removing && editing !== null}
        title={editing ? `Remove ${name(editing)}?` : "Remove?"}
        confirm="Remove"
        destructive
        busy={saving}
        onClose={() => setRemoving(false)}
        onConfirm={() => editing && void store(editing, null)}
      >
        {editing?.what === "scanner"
          ? "Its games are no longer offered after Pyroshine restarts."
          : "It is removed from the configuration file; clients stop seeing it after Pyroshine restarts."}
      </ConfirmDialog>
    </>
  );
}
