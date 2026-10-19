// The dialog the Applications page edits `[[application]]` and
// `[[application_scanner]]` entries in, and the scanner presets.

import { useState } from "react";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Dialog from "@mui/material/Dialog";
import DialogActions from "@mui/material/DialogActions";
import DialogContent from "@mui/material/DialogContent";
import DialogTitle from "@mui/material/DialogTitle";
import Divider from "@mui/material/Divider";
import FormControlLabel from "@mui/material/FormControlLabel";
import Stack from "@mui/material/Stack";
import Switch from "@mui/material/Switch";
import DeleteOutline from "@mui/icons-material/DeleteOutlineOutlined";
import type { FieldSpec } from "../api/types";
import { FieldControl, FieldRow } from "./fields";

type Item = Record<string, unknown>;

/// Missing required settings of an item, checked before the dialog closes.
function missing(fields: FieldSpec[], item: Item): string[] {
  return fields
    .filter((field) => field.required)
    .filter((field) => {
      const value = item[field.path];
      if (field.kind.type === "command") return !Array.isArray(value) || !String(value[0] ?? "").trim();
      if (field.kind.type === "path_list") return !Array.isArray(value) || value.every((path) => !String(path).trim());
      return value == null || String(value).trim() === "";
    })
    .map((field) => field.label);
}

export function ItemDialog({
  title,
  fields,
  initial,
  onSave,
  onClose,
  disabled,
  busy,
  error,
  saveLabel = "Done",
  onRemove,
}: {
  title: string;
  fields: FieldSpec[];
  initial: Item;
  onSave: (item: Item) => void;
  onClose: () => void;
  disabled?: boolean;
  busy?: boolean;
  /// Why the last save failed.
  error?: string | null;
  saveLabel?: string;
  onRemove?: () => void;
}) {
  const [item, setItem] = useState(initial);
  // Open the advanced settings when the entry uses hooks or output redirection.
  const [advanced, setAdvanced] = useState(() =>
    fields.some((field) => {
      const value = initial[field.path];
      return field.advanced && (typeof value === "string" || (Array.isArray(value) && value.length > 0));
    }),
  );
  const [tried, setTried] = useState(false);
  const absent = missing(fields, item);
  const visible = fields.filter((field) => advanced || !field.advanced);
  return (
    <Dialog open onClose={busy ? undefined : onClose} maxWidth="md" fullWidth>
      <DialogTitle>{title}</DialogTitle>
      <DialogContent dividers>
        {error && (
          <Alert severity="error" sx={{ mb: 2, overflowWrap: "anywhere" }}>
            {error}
          </Alert>
        )}
        {visible.map((field, index) => (
          <Box key={field.path}>
            {index > 0 && <Divider />}
            <FieldRow spec={field}>
              <FieldControl
                spec={field}
                value={item[field.path] ?? null}
                disabled={disabled}
                error={tried && absent.includes(field.label) ? "Required" : undefined}
                onChange={(value) => setItem((current) => {
                  const next = { ...current };
                  // An emptied setting without a value of its own falls back to its default.
                  if (value === null || value === undefined || (value === "" && !field.required && field.kind.type === "path")) delete next[field.path];
                  else next[field.path] = value;
                  return next;
                })}
              />
            </FieldRow>
          </Box>
        ))}
      </DialogContent>
      <DialogActions sx={{ px: 3, py: 2, justifyContent: "space-between" }}>
        <Stack direction="row" sx={{ gap: 1, alignItems: "center", flexWrap: "wrap" }}>
          <FormControlLabel control={<Switch checked={advanced} onChange={(event) => setAdvanced(event.target.checked)} />} label="Advanced settings" />
          {onRemove && (
            <Button color="error" startIcon={<DeleteOutline />} disabled={disabled || busy} onClick={onRemove}>
              Remove
            </Button>
          )}
        </Stack>
        <Stack direction="row" sx={{ gap: 1, alignItems: "center" }}>
          {tried && absent.length > 0 && <Alert severity="error" sx={{ py: 0 }}>Missing: {absent.join(", ")}</Alert>}
          <Button onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button
            variant="contained"
            disabled={disabled || busy}
            onClick={() => {
              setTried(true);
              if (absent.length === 0) onSave(item);
            }}
          >
            {saveLabel}
          </Button>
        </Stack>
      </DialogActions>
    </Dialog>
  );
}

/// Starting values for a new scanner, from the configuration reference.
export const scannerPresets: Record<string, Item> = {
  steam: { library: "$HOME/.local/share/Steam", command: ["/usr/bin/steam", "-bigpicture", "steam://rungameid/{game_id}"] },
  lutris: { command: ["/usr/bin/lutris", "lutris:rungame/{slug}"] },
  heroic: { command: ["/usr/bin/heroic", "heroic://launch?appName={app_name}&runner={runner}"] },
  desktop: { directories: ["$HOME/.local/share/applications", "/usr/share/applications"], include_terminal: false, resolve_icons: true },
};

/// A new scanner of `type` with its preset values.
export function newScanner(type: string): Item {
  return { type, launch_timeout_secs: 2, ...structuredClone(scannerPresets[type] ?? {}) };
}
