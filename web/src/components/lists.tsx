// Editors for `[[application]]` and `[[application_scanner]]` entries.

import { useState } from "react";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import Chip from "@mui/material/Chip";
import Dialog from "@mui/material/Dialog";
import DialogActions from "@mui/material/DialogActions";
import DialogContent from "@mui/material/DialogContent";
import DialogTitle from "@mui/material/DialogTitle";
import Divider from "@mui/material/Divider";
import FormControlLabel from "@mui/material/FormControlLabel";
import IconButton from "@mui/material/IconButton";
import ListItemText from "@mui/material/ListItemText";
import Menu from "@mui/material/Menu";
import MenuItem from "@mui/material/MenuItem";
import Stack from "@mui/material/Stack";
import Switch from "@mui/material/Switch";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import Add from "@mui/icons-material/Add";
import ArrowDownward from "@mui/icons-material/ArrowDownward";
import ArrowUpward from "@mui/icons-material/ArrowUpward";
import DeleteOutline from "@mui/icons-material/DeleteOutlineOutlined";
import EditOutlined from "@mui/icons-material/EditOutlined";
import type { FieldSpec, ScannerVariant } from "../api/types";
import { move } from "../config/draft";
import { FieldControl, FieldRow } from "./fields";

type Item = Record<string, unknown>;

function commandLine(command: unknown): string {
  if (!Array.isArray(command) || command.length === 0) return "(no command)";
  return command.map((arg) => (/[\s"']/.test(String(arg)) ? JSON.stringify(arg) : String(arg))).join(" ");
}

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

function ItemDialog({
  title,
  fields,
  initial,
  onSave,
  onClose,
  disabled,
}: {
  title: string;
  fields: FieldSpec[];
  initial: Item;
  onSave: (item: Item) => void;
  onClose: () => void;
  disabled?: boolean;
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
    <Dialog open onClose={onClose} maxWidth="md" fullWidth>
      <DialogTitle>{title}</DialogTitle>
      <DialogContent dividers>
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
        <FormControlLabel control={<Switch checked={advanced} onChange={(event) => setAdvanced(event.target.checked)} />} label="Advanced settings" />
        <Stack direction="row" sx={{ gap: 1, alignItems: "center" }}>
          {tried && absent.length > 0 && <Alert severity="error" sx={{ py: 0 }}>Missing: {absent.join(", ")}</Alert>}
          <Button onClick={onClose}>Cancel</Button>
          <Button
            variant="contained"
            disabled={disabled}
            onClick={() => {
              setTried(true);
              if (absent.length === 0) onSave(item);
            }}
          >
            Done
          </Button>
        </Stack>
      </DialogActions>
    </Dialog>
  );
}

function ListShell({
  items,
  render,
  onChange,
  onEdit,
  disabled,
  empty,
}: {
  items: Item[];
  render: (item: Item) => { title: string; subtitle: string; chips: string[] };
  onChange: (items: Item[]) => void;
  onEdit: (index: number) => void;
  disabled?: boolean;
  empty: string;
}) {
  if (items.length === 0) {
    return (
      <Card sx={{ p: 3, mb: 1.5 }}>
        <Typography color="text.secondary">{empty}</Typography>
      </Card>
    );
  }
  return (
    <Card sx={{ mb: 1.5 }}>
      {items.map((item, index) => {
        const { title, subtitle, chips } = render(item);
        return (
          <Box key={index}>
            {index > 0 && <Divider />}
            <Stack direction="row" sx={{ alignItems: "center", gap: 1, px: 2.5, py: 1.5 }}>
              <Box sx={{ flex: 1, minWidth: 0 }}>
                <Stack direction="row" sx={{ gap: 1, alignItems: "center", flexWrap: "wrap" }}>
                  <Typography variant="subtitle1" sx={{ fontWeight: 500 }}>
                    {title}
                  </Typography>
                  {chips.map((chip) => (
                    <Chip key={chip} label={chip} size="small" variant="outlined" sx={{ height: 22 }} />
                  ))}
                </Stack>
                <Typography variant="body2" color="text.secondary" noWrap sx={{ fontFamily: "monospace", fontSize: 12.5 }}>
                  {subtitle}
                </Typography>
              </Box>
              <Tooltip title="Move up">
                <span>
                  <IconButton size="small" aria-label="Move up" disabled={disabled || index === 0} onClick={() => onChange(move(items, index, index - 1))}>
                    <ArrowUpward fontSize="small" />
                  </IconButton>
                </span>
              </Tooltip>
              <Tooltip title="Move down">
                <span>
                  <IconButton size="small" aria-label="Move down" disabled={disabled || index === items.length - 1} onClick={() => onChange(move(items, index, index + 1))}>
                    <ArrowDownward fontSize="small" />
                  </IconButton>
                </span>
              </Tooltip>
              <Tooltip title="Edit">
                <IconButton size="small" aria-label={`Edit ${title}`} onClick={() => onEdit(index)}>
                  <EditOutlined fontSize="small" />
                </IconButton>
              </Tooltip>
              <Tooltip title="Remove">
                <span>
                  <IconButton size="small" aria-label={`Remove ${title}`} disabled={disabled} onClick={() => onChange(items.filter((_, i) => i !== index))}>
                    <DeleteOutline fontSize="small" />
                  </IconButton>
                </span>
              </Tooltip>
            </Stack>
          </Box>
        );
      })}
    </Card>
  );
}

export function ApplicationsEditor({ fields, value, onChange, disabled }: { fields: FieldSpec[]; value: Item[]; onChange: (value: Item[]) => void; disabled?: boolean }) {
  const [editing, setEditing] = useState<number | "new" | null>(null);
  return (
    <>
      <ListShell
        items={value}
        disabled={disabled}
        onChange={onChange}
        onEdit={setEditing}
        empty="No static applications. Clients only see discovered applications."
        render={(item) => ({
          title: String(item.title || "Untitled"),
          subtitle: commandLine(item.command),
          chips: [
            ...(item.output_scale != null ? [`scale ${item.output_scale}`] : []),
            ...(Array.isArray(item.pre_command) && item.pre_command.length ? ["pre-launch"] : []),
            ...(Array.isArray(item.post_command) && item.post_command.length ? ["post-session"] : []),
          ],
        })}
      />
      <Button startIcon={<Add />} disabled={disabled} onClick={() => setEditing("new")}>
        Add application
      </Button>
      {editing !== null && (
        <ItemDialog
          title={editing === "new" ? "Add application" : "Edit application"}
          fields={fields}
          disabled={disabled}
          initial={editing === "new" ? { title: "", command: [""], launch_timeout_secs: 2 } : value[editing]}
          onClose={() => setEditing(null)}
          onSave={(item) => {
            onChange(editing === "new" ? [...value, item] : value.map((existing, index) => (index === editing ? item : existing)));
            setEditing(null);
          }}
        />
      )}
    </>
  );
}

/// Starting values for a new scanner, from the configuration reference.
const presets: Record<string, Item> = {
  steam: { library: "$HOME/.local/share/Steam", command: ["/usr/bin/steam", "-bigpicture", "steam://rungameid/{game_id}"] },
  lutris: { command: ["/usr/bin/lutris", "lutris:rungame/{slug}"] },
  heroic: { command: ["/usr/bin/heroic", "heroic://launch?appName={app_name}&runner={runner}"] },
  desktop: { directories: ["$HOME/.local/share/applications", "/usr/share/applications"], include_terminal: false, resolve_icons: true },
};

export function ScannersEditor({ variants, value, onChange, disabled }: { variants: ScannerVariant[]; value: Item[]; onChange: (value: Item[]) => void; disabled?: boolean }) {
  const [editing, setEditing] = useState<{ index: number | "new"; item: Item } | null>(null);
  const [menu, setMenu] = useState<HTMLElement | null>(null);
  const variant = (item: Item) => variants.find((candidate) => candidate.id === item.type);
  return (
    <>
      <ListShell
        items={value}
        disabled={disabled}
        onChange={onChange}
        onEdit={(index) => setEditing({ index, item: value[index] })}
        empty="No scanners. Only static applications are offered."
        render={(item) => {
          const kind = variant(item);
          const source = item.library ?? item.pga_db ?? item.config_dir ?? (Array.isArray(item.directories) ? item.directories.join(", ") : "");
          return {
            title: kind?.label ?? String(item.type),
            subtitle: [source, item.command ? commandLine(item.command) : ""].filter(Boolean).join("  ·  "),
            chips: [],
          };
        }}
      />
      <Button startIcon={<Add />} disabled={disabled} onClick={(event) => setMenu(event.currentTarget)}>
        Add scanner
      </Button>
      <Menu anchorEl={menu} open={menu !== null} onClose={() => setMenu(null)}>
        {variants.map((candidate) => (
          <MenuItem
            key={candidate.id}
            onClick={() => {
              setMenu(null);
              setEditing({ index: "new", item: { type: candidate.id, launch_timeout_secs: 2, ...structuredClone(presets[candidate.id] ?? {}) } });
            }}
          >
            <ListItemText primary={candidate.label} secondary={candidate.description} />
          </MenuItem>
        ))}
      </Menu>
      {editing && variant(editing.item) && (
        <ItemDialog
          title={`${editing.index === "new" ? "Add" : "Edit"} ${variant(editing.item)!.label} scanner`}
          fields={variant(editing.item)!.fields}
          initial={editing.item}
          disabled={disabled}
          onClose={() => setEditing(null)}
          onSave={(item) => {
            const next = { ...item, type: editing.item.type };
            onChange(editing.index === "new" ? [...value, next] : value.map((existing, index) => (index === editing.index ? next : existing)));
            setEditing(null);
          }}
        />
      )}
    </>
  );
}
