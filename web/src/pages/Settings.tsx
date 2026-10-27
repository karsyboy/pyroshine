import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Alert from "@mui/material/Alert";
import AlertTitle from "@mui/material/AlertTitle";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import ButtonBase from "@mui/material/ButtonBase";
import Card from "@mui/material/Card";
import CircularProgress from "@mui/material/CircularProgress";
import Divider from "@mui/material/Divider";
import FormControlLabel from "@mui/material/FormControlLabel";
import Paper from "@mui/material/Paper";
import Stack from "@mui/material/Stack";
import Switch from "@mui/material/Switch";
import Typography from "@mui/material/Typography";
import RestartAlt from "@mui/icons-material/RestartAlt";
import { api, errorKind, errorMessage } from "../api/client";
import type { ConfigDocument, ConfigIssue, ConfigSchema, ConfigValues, FieldSpec } from "../api/types";
import { ConfirmDialog, PageHeader } from "../components/common";
import { RestartButton } from "../components/Restart";
import { FieldControl, FieldRow } from "../components/fields";
import { changedFields, getPath, issueField, normalize, setPath } from "../config/draft";
import { useDaemon } from "../state/daemon";
import { setPreference, usePreferences } from "../state/preferences";

/// Sections edited on another page: static applications, scanners and
/// discovery options live on the Applications page, beside what they produce.
const ELSEWHERE = new Set(["applications", "scanners"]);

function RestartNotice({ saved }: { saved: boolean }) {
  return (
    <Alert severity="info" icon={<RestartAlt />} sx={{ mb: 3 }}>
      <AlertTitle>{saved ? "Configuration saved. Restart Pyroshine to apply changes." : "Restart Pyroshine to apply the saved configuration."}</AlertTitle>
      Pyroshine reads its configuration when it starts. Restarting ends any active session.
      <Box sx={{ mt: 1.5 }}>
        <RestartButton variant="contained" label="Restart now" />
      </Box>
    </Alert>
  );
}

/// Preferences of this browser shown beside the server's diagnostics
/// settings. They apply at once and are not part of the configuration file.
function DiagnosticsPreferences() {
  const { infoFindings } = usePreferences();
  return (
    <Box sx={{ mt: 3 }}>
      <Typography variant="subtitle1" sx={{ fontWeight: 600 }}>
        This browser
      </Typography>
      <Typography variant="body2" color="text.secondary" sx={{ mb: 1.5 }}>
        Saved in this browser and applied immediately; no restart needed.
      </Typography>
      <Card sx={{ px: 3 }}>
        <Stack direction={{ xs: "column", md: "row" }} sx={{ gap: { xs: 1, md: 3 }, py: 2, alignItems: { md: "center" } }}>
          <Box sx={{ flex: 1, minWidth: 0 }}>
            <Typography variant="subtitle2">Informational findings</Typography>
            <Typography variant="body2" color="text.secondary" sx={{ mt: 0.25 }}>
              Show the dashboard's informational findings, such as a game rendering below the stream rate or composited
              frames. Warnings are always shown, and Copy diagnostics still includes every finding.
            </Typography>
          </Box>
          <Switch
            checked={infoFindings}
            onChange={(event) => setPreference("infoFindings", event.target.checked)}
            slotProps={{ input: { "aria-label": "Informational findings" } }}
          />
        </Stack>
      </Card>
    </Box>
  );
}

function SectionNav({
  schema,
  section,
  setSection,
  changed,
  issues,
}: {
  schema: ConfigSchema;
  section: string;
  setSection: (id: string) => void;
  changed: Set<string>;
  issues: Set<string>;
}) {
  const nav = useRef<HTMLElement>(null);
  // Keep the selected section visible in the phone strip. Only the strip
  // scrolls: scrollIntoView would also move the page while editing.
  useEffect(() => {
    const strip = nav.current;
    const selected = strip?.querySelector<HTMLElement>("[aria-current=page]");
    if (!strip || !selected || strip.scrollWidth <= strip.clientWidth) return;
    const left = selected.offsetLeft - strip.offsetLeft;
    if (left < strip.scrollLeft || left + selected.offsetWidth > strip.scrollLeft + strip.clientWidth) {
      strip.scrollLeft = left - (strip.clientWidth - selected.offsetWidth) / 2;
    }
  }, [section]);
  return (
    // Below md the list becomes a horizontally scrolling strip above the
    // fields, so it does not take half of a phone's width.
    <Stack
      ref={nav}
      component="nav"
      aria-label="Settings sections"
      direction={{ xs: "row", md: "column" }}
      sx={{
        width: { xs: "100%", md: 220 },
        flexShrink: 0,
        gap: 0.5,
        position: { md: "sticky" },
        top: 0,
        alignSelf: "flex-start",
        overflowX: { xs: "auto", md: "visible" },
        scrollbarWidth: "none",
      }}
    >
      {schema.sections.map((candidate) => {
        const selected = candidate.id === section;
        const fields = schema.fields.filter((field) => field.section === candidate.id);
        const dirty = fields.some((field) => changed.has(field.path));
        const invalid = fields.some((field) => issues.has(field.path));
        return (
          <ButtonBase
            key={candidate.id}
            onClick={() => setSection(candidate.id)}
            aria-current={selected ? "page" : undefined}
            sx={{
              flexShrink: 0,
              gap: 1,
              whiteSpace: "nowrap",
              justifyContent: "space-between",
              px: 2,
              py: 1.25,
              borderRadius: 7,
              bgcolor: selected ? "action.selected" : "transparent",
              "&:hover": { bgcolor: "action.hover" },
            }}
          >
            <Typography variant="body2" sx={{ fontWeight: selected ? 700 : 500 }}>
              {candidate.title}
            </Typography>
            {(dirty || invalid) && (
              <Box sx={{ width: 8, height: 8, borderRadius: "50%", bgcolor: invalid ? "error.main" : "primary.main" }} aria-label={invalid ? "has errors" : "changed"} />
            )}
          </ButtonBase>
        );
      })}
    </Stack>
  );
}

export function SettingsPage() {
  const daemon = useDaemon();
  const [document, setDocument] = useState<ConfigDocument | null>(null);
  const [schema, setSchema] = useState<ConfigSchema | null>(null);
  const [draft, setDraft] = useState<ConfigValues | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [section, setSection] = useState("general");
  const [advanced, setAdvanced] = useState(false);
  const [issues, setIssues] = useState<ConfigIssue[]>([]);
  const [saving, setSaving] = useState(false);
  const [justSaved, setJustSaved] = useState(false);
  const [conflict, setConflict] = useState(false);
  const [discard, setDiscard] = useState(false);

  const load = useCallback(async () => {
    setLoadError(null);
    try {
      const loaded = await api.loadConfig();
      setDocument(loaded.document);
      setSchema(loaded.schema);
      setDraft(loaded.document.values);
      setIssues([]);
    } catch (error) {
      setLoadError(errorMessage(error));
    }
  }, []);

  useEffect(() => {
    if (daemon.connected) void load();
  }, [daemon.connected, load]);

  const changed = useMemo(
    () => new Set(schema && document && draft ? changedFields(schema.fields, document.values, draft) : []),
    [schema, document, draft],
  );

  // After a restart the running configuration is the saved one.
  const startedAt = daemon.server?.started_at_ms;
  const seenStart = useRef(startedAt);
  useEffect(() => {
    if (startedAt === undefined || startedAt === seenStart.current) return;
    const restarted = seenStart.current !== undefined;
    seenStart.current = startedAt;
    if (restarted && changed.size === 0) {
      setJustSaved(false);
      void load();
    }
  }, [startedAt, changed.size, load]);

  // Another editor (or another window) saved the file while this page is open.
  useEffect(() => {
    if (!daemon.configRevision || !document || daemon.configRevision === document.revision || saving) return;
    if (changed.size === 0) void load();
    else setConflict(true);
  }, [daemon.configRevision, document, changed.size, load, saving]);

  if (loadError) {
    return (
      <>
        <PageHeader title="Settings" />
        <Alert severity="error" action={<Button onClick={() => void load()}>Retry</Button>}>
          {loadError}
        </Alert>
      </>
    );
  }
  if (!document || !schema || !draft) {
    return (
      <>
        <PageHeader title="Settings" />
        <CircularProgress />
      </>
    );
  }

  const readOnly = !document.writable;
  const issueFields = new Set(issues.map((issue) => issueField(issue.path)).filter((path): path is string => path !== null));
  const sections = { ...schema, sections: schema.sections.filter((candidate) => !ELSEWHERE.has(candidate.id)) };
  const current = sections.sections.find((candidate) => candidate.id === section) ?? sections.sections[0];
  const fields = schema.fields.filter((field) => field.section === current.id && (advanced || !field.advanced || changed.has(field.path) || issueFields.has(field.path)));
  const hiddenAdvanced = schema.fields.filter((field) => field.section === current.id && field.advanced).length - fields.filter((field) => field.advanced).length;
  const fieldError = (field: FieldSpec) =>
    issues
      .filter((issue) => issueField(issue.path) === field.path)
      .map((issue) => (issue.path === field.path ? issue.message : `${issue.path}: ${issue.message}`))
      .join("; ") || undefined;
  const update = (path: string, value: unknown) => {
    setDraft((values) => (values ? setPath(values, path, value) : values));
    setJustSaved(false);
  };

  const save = async () => {
    setSaving(true);
    setIssues([]);
    try {
      const values = normalize(draft, schema.fields);
      const report = await api.validateConfig(values);
      if (!report.valid) {
        setIssues(report.issues);
        const first = report.issues.map((issue) => issueField(issue.path)).find(Boolean);
        const target = schema.fields.find((field) => field.path === first);
        if (target && !ELSEWHERE.has(target.section)) setSection(target.section);
        return;
      }
      const outcome = await api.saveConfig(values, document.revision);
      await load();
      setJustSaved(outcome.restart_required);
      daemon.notify("success", outcome.restart_required ? "Configuration saved. Restart Pyroshine to apply it." : "Configuration saved.");
    } catch (error) {
      if (errorKind(error) === "conflict") setConflict(true);
      else if (errorKind(error) === "invalid") setIssues([{ path: null, message: errorMessage(error) }]);
      else daemon.notify("error", errorMessage(error));
    } finally {
      setSaving(false);
    }
  };

  const generalIssues = issues.filter(
    (issue) =>
      !issueField(issue.path) ||
      !schema.fields.some((field) => field.path === issueField(issue.path) && !ELSEWHERE.has(field.section)),
  );

  return (
    <Box sx={{ pb: changed.size > 0 ? 10 : 0 }}>
      <PageHeader
        title="Settings"
        subtitle={<Box component="span" sx={{ fontFamily: "monospace" }}>{document.path}</Box>}
        actions={<FormControlLabel control={<Switch checked={advanced} onChange={(event) => setAdvanced(event.target.checked)} />} label="Show advanced" />}
      />
      {readOnly && (
        <Alert severity="warning" sx={{ mb: 3 }}>
          <AlertTitle>Read-only configuration</AlertTitle>
          {document.read_only_reason}
        </Alert>
      )}
      {(justSaved || (document.restart_required && changed.size === 0)) && <RestartNotice saved={justSaved} />}
      {generalIssues.length > 0 && (
        <Alert severity="error" sx={{ mb: 3 }}>
          <AlertTitle>The configuration was not saved</AlertTitle>
          {generalIssues.map((issue, index) => (
            <div key={index}>{issue.path ? `${issue.path}: ${issue.message}` : issue.message}</div>
          ))}
        </Alert>
      )}

      <Stack direction={{ xs: "column", md: "row" }} sx={{ gap: { xs: 2, md: 3 }, alignItems: "flex-start" }}>
        <SectionNav schema={sections} section={current.id} setSection={setSection} changed={changed} issues={issueFields} />
        <Box sx={{ flex: 1, minWidth: 0 }}>
          <Typography variant="h5">{current.title}</Typography>
          <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
            {current.description}
          </Typography>
          {fields.some((field) => field.kind.type !== "applications" && field.kind.type !== "scanners") && (
            <Card sx={{ px: 3 }}>
              {fields
                .filter((field) => field.kind.type !== "applications" && field.kind.type !== "scanners")
                .map((field, index) => (
                  <Box key={field.path}>
                    {index > 0 && <Divider />}
                    <FieldRow spec={field} changed={changed.has(field.path)}>
                      <FieldControl
                        spec={field}
                        value={getPath(draft, field.path) ?? null}
                        onChange={(value) => update(field.path, value)}
                        error={fieldError(field)}
                        disabled={readOnly}
                      />
                    </FieldRow>
                  </Box>
                ))}
            </Card>
          )}
          {hiddenAdvanced > 0 && (
            <Button size="small" sx={{ mt: 1.5 }} onClick={() => setAdvanced(true)}>
              Show {hiddenAdvanced} advanced {hiddenAdvanced === 1 ? "setting" : "settings"}
            </Button>
          )}
          {current.id === "diagnostics" && <DiagnosticsPreferences />}
        </Box>
      </Stack>

      {changed.size > 0 && (
        <Paper
          elevation={6}
          sx={{
            position: "fixed",
            bottom: 24,
            left: "50%",
            transform: "translateX(calc(-50% + 48px))",
            px: 3,
            py: 1.5,
            borderRadius: 7,
            display: "flex",
            alignItems: "center",
            gap: 2,
            zIndex: 1200,
          }}
        >
          <Typography variant="body2">
            {changed.size} unsaved {changed.size === 1 ? "change" : "changes"}
          </Typography>
          <Button color="inherit" onClick={() => setDiscard(true)} disabled={saving}>
            Discard
          </Button>
          <Button variant="contained" onClick={() => void save()} disabled={saving || readOnly}>
            {saving ? "Saving…" : "Save"}
          </Button>
        </Paper>
      )}

      <ConfirmDialog
        open={discard}
        title="Discard changes?"
        confirm="Discard"
        destructive
        onConfirm={() => {
          setDraft(document.values);
          setIssues([]);
          setDiscard(false);
        }}
        onClose={() => setDiscard(false)}
      >
        Your edits since the configuration was loaded will be lost.
      </ConfirmDialog>
      <ConfirmDialog
        open={conflict}
        title="The configuration changed"
        confirm="Reload"
        onConfirm={() => {
          setConflict(false);
          void load();
        }}
        onClose={() => setConflict(false)}
      >
        The configuration file was changed outside this page since it was loaded. Reload it and apply your changes
        again; nothing was overwritten.
      </ConfirmDialog>
    </Box>
  );
}
