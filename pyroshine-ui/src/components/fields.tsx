// One control per schema field kind. Values are the daemon's typed JSON:
// `null` for an unset optional setting.

import Autocomplete from "@mui/material/Autocomplete";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Chip from "@mui/material/Chip";
import FormHelperText from "@mui/material/FormHelperText";
import IconButton from "@mui/material/IconButton";
import InputAdornment from "@mui/material/InputAdornment";
import MenuItem from "@mui/material/MenuItem";
import Stack from "@mui/material/Stack";
import Switch from "@mui/material/Switch";
import TextField from "@mui/material/TextField";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import Add from "@mui/icons-material/Add";
import Close from "@mui/icons-material/Close";
import FolderOutlined from "@mui/icons-material/FolderOutlined";
import WarningAmber from "@mui/icons-material/WarningAmber";
import { useState, type ReactNode } from "react";
import type { FieldSpec } from "../api/types";

export interface ControlProps {
  spec: FieldSpec;
  value: unknown;
  onChange: (value: unknown) => void;
  error?: string;
  disabled?: boolean;
}

function NumberControl({ spec, value, onChange, error, disabled }: ControlProps) {
  const kind = spec.kind;
  const [text, setText] = useState(value == null ? "" : String(value));
  const [lastValue, setLastValue] = useState(value);
  if (value !== lastValue) {
    setLastValue(value);
    setText(value == null ? "" : String(value));
  }
  const bounds = kind.type === "port" ? { min: 1, max: 65535 } : kind.type === "integer" || kind.type === "number" ? kind : null;
  const integer = kind.type !== "number";
  const optional = (kind.type === "integer" || kind.type === "number") && kind.optional;
  const unit = kind.type === "integer" || kind.type === "number" ? kind.unit : null;
  const parsed = text.trim() === "" ? null : Number(text);
  const invalid =
    (parsed === null && !optional) ||
    (parsed !== null && (!Number.isFinite(parsed) || (integer && !Number.isInteger(parsed)) || (bounds && (parsed < bounds.min || parsed > bounds.max))));
  return (
    <TextField
      value={text}
      disabled={disabled}
      type="number"
      error={Boolean(error) || Boolean(invalid)}
      helperText={error ?? (invalid ? (bounds ? `Between ${bounds.min} and ${bounds.max}` : "Enter a number") : optional ? "Leave empty for the default" : " ")}
      onChange={(event) => {
        setText(event.target.value);
        const next = event.target.value.trim() === "" ? null : Number(event.target.value);
        if (next === null) {
          if (optional) onChange(null);
        } else if (Number.isFinite(next)) {
          onChange(next);
        }
      }}
      slotProps={{
        htmlInput: {
          min: bounds?.min,
          max: bounds?.max,
          step: kind.type === "number" ? kind.step : 1,
          "aria-label": spec.label,
        },
        input: unit ? { endAdornment: <InputAdornment position="end">{unit}</InputAdornment> } : undefined,
      }}
    />
  );
}

/// Arguments of one command, one per row: what systemd executes verbatim.
export function CommandEditor({
  value,
  onChange,
  disabled,
  placeholders = [],
  label = "Command",
}: {
  value: string[];
  onChange: (value: string[]) => void;
  disabled?: boolean;
  placeholders?: string[];
  label?: string;
}) {
  const args = value.length === 0 ? [""] : value;
  const set = (index: number, text: string) => onChange(args.map((arg, i) => (i === index ? text : arg)));
  return (
    <Stack sx={{ gap: 1 }}>
      {args.map((arg, index) => (
        <Stack key={index} direction="row" sx={{ gap: 1, alignItems: "center" }}>
          <TextField
            value={arg}
            disabled={disabled}
            placeholder={index === 0 ? "/usr/bin/program" : "argument"}
            onChange={(event) => set(index, event.target.value)}
            slotProps={{
              htmlInput: { "aria-label": index === 0 ? `${label}: executable` : `${label}: argument ${index}`, spellCheck: false },
              input: {
                startAdornment: (
                  <InputAdornment position="start">
                    <Typography variant="caption" color="text.secondary" sx={{ width: 28 }}>
                      {index === 0 ? "exec" : `$${index}`}
                    </Typography>
                  </InputAdornment>
                ),
              },
            }}
            sx={{ "& input": { fontFamily: "monospace", fontSize: 13 } }}
          />
          <IconButton
            size="small"
            aria-label="Remove argument"
            disabled={disabled || args.length === 1}
            onClick={() => onChange(args.filter((_, i) => i !== index))}
          >
            <Close fontSize="small" />
          </IconButton>
        </Stack>
      ))}
      <Stack direction="row" sx={{ gap: 1, alignItems: "center", flexWrap: "wrap" }}>
        <Button size="small" startIcon={<Add />} disabled={disabled} onClick={() => onChange([...args, ""])}>
          Add argument
        </Button>
        {placeholders.map((placeholder) => (
          <Tooltip key={placeholder} title="Replaced for each discovered application">
            <Chip size="small" variant="outlined" label={placeholder} sx={{ fontFamily: "monospace" }} />
          </Tooltip>
        ))}
      </Stack>
    </Stack>
  );
}

function CommandListEditor({ value, onChange, disabled }: { value: string[][]; onChange: (value: string[][]) => void; disabled?: boolean }) {
  return (
    <Stack sx={{ gap: 1.5 }}>
      {value.map((command, index) => (
        <Box key={index} sx={{ border: 1, borderColor: "divider", borderRadius: 2, p: 1.5 }}>
          <Stack direction="row" sx={{ justifyContent: "space-between", alignItems: "center", mb: 1 }}>
            <Typography variant="caption" color="text.secondary">
              Command {index + 1}
            </Typography>
            <IconButton size="small" aria-label="Remove command" disabled={disabled} onClick={() => onChange(value.filter((_, i) => i !== index))}>
              <Close fontSize="small" />
            </IconButton>
          </Stack>
          <CommandEditor value={command} disabled={disabled} label={`Command ${index + 1}`} onChange={(next) => onChange(value.map((c, i) => (i === index ? next : c)))} />
        </Box>
      ))}
      <Box>
        <Button size="small" startIcon={<Add />} disabled={disabled} onClick={() => onChange([...value, [""]])}>
          Add command
        </Button>
      </Box>
    </Stack>
  );
}

function PathListEditor({ value, onChange, disabled }: { value: string[]; onChange: (value: string[]) => void; disabled?: boolean }) {
  const paths = value.length === 0 ? [""] : value;
  return (
    <Stack sx={{ gap: 1 }}>
      {paths.map((path, index) => (
        <Stack key={index} direction="row" sx={{ gap: 1, alignItems: "center" }}>
          <TextField
            value={path}
            disabled={disabled}
            placeholder="$HOME/.local/share/applications"
            onChange={(event) => onChange(paths.map((p, i) => (i === index ? event.target.value : p)))}
            slotProps={{
              htmlInput: { "aria-label": `Directory ${index + 1}`, spellCheck: false },
              input: { startAdornment: <InputAdornment position="start"><FolderOutlined fontSize="small" /></InputAdornment> },
            }}
          />
          <IconButton size="small" aria-label="Remove directory" disabled={disabled || paths.length === 1} onClick={() => onChange(paths.filter((_, i) => i !== index))}>
            <Close fontSize="small" />
          </IconButton>
        </Stack>
      ))}
      <Box>
        <Button size="small" startIcon={<Add />} disabled={disabled} onClick={() => onChange([...paths, ""])}>
          Add directory
        </Button>
      </Box>
    </Stack>
  );
}

/// Control for a scalar or list field. Structured lists (applications,
/// scanners) have their own editors.
export function FieldControl(props: ControlProps) {
  const { spec, value, onChange, error, disabled } = props;
  const kind = spec.kind;
  switch (kind.type) {
    case "bool":
      return (
        <Switch
          checked={Boolean(value)}
          disabled={disabled}
          onChange={(event) => onChange(event.target.checked)}
          slotProps={{ input: { "aria-label": spec.label } }}
        />
      );
    case "choice":
      return (
        <TextField
          select
          value={value == null ? "" : String(value)}
          disabled={disabled}
          error={Boolean(error)}
          helperText={error ?? kind.options.find((option) => option.value === value)?.description ?? " "}
          onChange={(event) => onChange(event.target.value === "" ? null : event.target.value)}
          slotProps={{
            htmlInput: { "aria-label": spec.label },
            select: {
              displayEmpty: true,
              renderValue: (selected) =>
                selected === "" ? kind.unset_label ?? "" : kind.options.find((option) => option.value === selected)?.label ?? String(selected),
            },
          }}
        >
          {kind.unset_label && <MenuItem value="">{kind.unset_label}</MenuItem>}
          {kind.options.map((option) => (
            <MenuItem key={option.value} value={option.value}>
              <Stack>
                <span>{option.label}</span>
                <Typography variant="caption" color="text.secondary" sx={{ whiteSpace: "normal" }}>
                  {option.description}
                </Typography>
              </Stack>
            </MenuItem>
          ))}
        </TextField>
      );
    case "integer":
    case "number":
    case "port":
      return <NumberControl {...props} />;
    case "text": {
      const set = (text: string) => onChange(kind.optional && text === "" ? null : text);
      const text = value == null ? "" : String(value);
      if (kind.suggestions.length > 0) {
        return (
          <Autocomplete
            freeSolo
            options={kind.suggestions}
            value={text}
            disabled={disabled}
            onInputChange={(_, input) => set(input)}
            renderInput={(params) => (
              <TextField
                {...params}
                placeholder={kind.placeholder ?? undefined}
                error={Boolean(error)}
                helperText={error ?? (kind.optional ? "Leave empty to unset" : " ")}
                slotProps={{ ...params.slotProps, htmlInput: { ...params.slotProps.htmlInput, "aria-label": spec.label, spellCheck: false } }}
              />
            )}
          />
        );
      }
      return (
        <TextField
          value={text}
          disabled={disabled}
          placeholder={kind.placeholder ?? undefined}
          error={Boolean(error)}
          helperText={error ?? (kind.optional ? "Leave empty to unset" : " ")}
          onChange={(event) => set(event.target.value)}
          slotProps={{ htmlInput: { "aria-label": spec.label, spellCheck: false } }}
        />
      );
    }
    case "path":
      return (
        <TextField
          value={value == null ? "" : String(value)}
          disabled={disabled}
          error={Boolean(error)}
          helperText={error ?? (kind.expands ? "~ and $VARIABLES are expanded" : kind.optional ? "Leave empty to unset" : " ")}
          onChange={(event) => onChange(kind.optional && event.target.value === "" ? null : event.target.value)}
          slotProps={{
            htmlInput: { "aria-label": spec.label, spellCheck: false },
            input: { startAdornment: <InputAdornment position="start"><FolderOutlined fontSize="small" /></InputAdornment> },
          }}
        />
      );
    case "command":
      return (
        <>
          <CommandEditor value={(value as string[]) ?? []} disabled={disabled} placeholders={kind.placeholders} label={spec.label} onChange={onChange} />
          {error && <FormHelperText error>{error}</FormHelperText>}
        </>
      );
    case "command_list":
      return <CommandListEditor value={(value as string[][]) ?? []} disabled={disabled} onChange={onChange} />;
    case "path_list":
      return (
        <>
          <PathListEditor value={(value as string[]) ?? []} disabled={disabled} onChange={onChange} />
          {error && <FormHelperText error>{error}</FormHelperText>}
        </>
      );
    default:
      return null;
  }
}

/// Label, help and markers for one field, with its control.
export function FieldRow({ spec, changed, children }: { spec: FieldSpec; changed?: boolean; children: ReactNode }) {
  const inline = spec.kind.type === "bool";
  const wide = ["command", "command_list", "path_list"].includes(spec.kind.type);
  return (
    <Stack
      direction={{ xs: "column", md: inline ? "row" : wide ? "column" : "row" }}
      sx={{ gap: { xs: 1, md: wide ? 1.5 : 3 }, py: 2, alignItems: { md: inline ? "center" : "flex-start" } }}
    >
      <Box sx={{ flex: 1, minWidth: 0 }}>
        <Stack direction="row" sx={{ alignItems: "center", gap: 1, flexWrap: "wrap" }}>
          <Typography variant="subtitle2">{spec.label}</Typography>
          {spec.advanced && <Chip label="Advanced" size="small" variant="outlined" sx={{ height: 20, fontSize: 11 }} />}
          {changed && <Chip label="Changed" size="small" color="primary" sx={{ height: 20, fontSize: 11 }} />}
        </Stack>
        <Typography variant="body2" color="text.secondary" sx={{ mt: 0.25 }}>
          {spec.help}
        </Typography>
        {spec.caution && (
          <Stack direction="row" sx={{ gap: 0.5, mt: 0.5, alignItems: "center", color: "warning.main" }}>
            <WarningAmber sx={{ fontSize: 16 }} />
            <Typography variant="caption">{spec.caution}</Typography>
          </Stack>
        )}
        <Typography variant="caption" color="text.disabled" sx={{ fontFamily: "monospace" }}>
          {spec.path}
        </Typography>
      </Box>
      <Box sx={{ width: { xs: "100%", md: inline ? "auto" : wide ? "100%" : 380 }, flexShrink: 0 }}>{children}</Box>
    </Stack>
  );
}
