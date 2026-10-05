import { useCallback, useEffect, useMemo, useState } from "react";
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
import { call, errorKind, errorMessage } from "../api/bridge";
import type { ConfigDocument, ConfigIssue, ConfigSchema, ConfigValues, FieldSpec, SaveOutcome, ValidationReport } from "../api/types";
import { ConfirmDialog, CopyableCommand, PageHeader } from "../components/common";
import { FieldControl, FieldRow } from "../components/fields";
import { ApplicationsEditor, ScannersEditor } from "../components/lists";
import { changedFields, getPath, issueField, normalize, setPath } from "../config/draft";
import { useDaemon } from "../state/daemon";

function RestartNotice({ saved }: { saved: boolean }) {
  return (
    <Alert severity="info" icon={<RestartAlt />} sx={{ mb: 3 }}>
      <AlertTitle>{saved ? "Configuration saved. Restart Pyroshine to apply changes." : "Restart Pyroshine to apply the saved configuration."}</AlertTitle>
      Pyroshine reads its configuration when it starts. Restarting ends any active session. For the packaged service:
      <Box sx={{ mt: 1, maxWidth: 520 }}>
        <CopyableCommand command='sudo systemctl restart "pyroshine@$USER"' />
      </Box>
    </Alert>
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
  return (
    <Stack component="nav" aria-label="Settings sections" sx={{ width: 220, flexShrink: 0, gap: 0.5, position: "sticky", top: 0, alignSelf: "flex-start" }}>
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
      const loaded = await call<{ document: ConfigDocument; schema: ConfigSchema }>("load_config");
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
  const current = schema.sections.find((candidate) => candidate.id === section) ?? schema.sections[0];
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
      const report = await call<ValidationReport>("validate_config", { values });
      if (!report.valid) {
        setIssues(report.issues);
        const first = report.issues.map((issue) => issueField(issue.path)).find(Boolean);
        const target = schema.fields.find((field) => field.path === first);
        if (target) setSection(target.section);
        return;
      }
      const outcome = await call<SaveOutcome>("save_config", { values, revision: document.revision });
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

  const generalIssues = issues.filter((issue) => !issueField(issue.path) || !schema.fields.some((field) => field.path === issueField(issue.path)));

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

      <Stack direction="row" sx={{ gap: 3, alignItems: "flex-start" }}>
        <SectionNav schema={schema} section={current.id} setSection={setSection} changed={changed} issues={issueFields} />
        <Box sx={{ flex: 1, minWidth: 0 }}>
          <Typography variant="h5">{current.title}</Typography>
          <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
            {current.description}
          </Typography>
          {fields.map((field) =>
            field.kind.type === "applications" ? (
              <Box key={field.path} sx={{ mb: 2 }}>
                <Typography variant="body2" color="text.secondary" sx={{ mb: 1.5 }}>
                  {field.help}
                </Typography>
                {fieldError(field) && <Alert severity="error" sx={{ mb: 1.5 }}>{fieldError(field)}</Alert>}
                <ApplicationsEditor
                  fields={field.kind.item}
                  value={(getPath(draft, field.path) as Record<string, unknown>[]) ?? []}
                  onChange={(value) => update(field.path, value)}
                  disabled={readOnly}
                />
              </Box>
            ) : field.kind.type === "scanners" ? (
              <Box key={field.path} sx={{ mb: 2 }}>
                <Typography variant="body2" color="text.secondary" sx={{ mb: 1.5 }}>
                  {field.help}
                </Typography>
                {fieldError(field) && <Alert severity="error" sx={{ mb: 1.5 }}>{fieldError(field)}</Alert>}
                <ScannersEditor
                  variants={field.kind.variants}
                  value={(getPath(draft, field.path) as Record<string, unknown>[]) ?? []}
                  onChange={(value) => update(field.path, value)}
                  disabled={readOnly}
                />
              </Box>
            ) : null,
          )}
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
        The configuration file was changed outside this window since it was loaded. Reload it and apply your changes
        again; nothing was overwritten.
      </ConfirmDialog>
    </Box>
  );
}
