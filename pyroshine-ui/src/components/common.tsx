import { useState, type ReactNode } from "react";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Dialog from "@mui/material/Dialog";
import DialogActions from "@mui/material/DialogActions";
import DialogContent from "@mui/material/DialogContent";
import DialogContentText from "@mui/material/DialogContentText";
import DialogTitle from "@mui/material/DialogTitle";
import IconButton from "@mui/material/IconButton";
import Stack from "@mui/material/Stack";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import ContentCopy from "@mui/icons-material/ContentCopy";
import Check from "@mui/icons-material/Check";
import type { SessionPhase } from "../api/types";

export function PageHeader({ title, subtitle, actions }: { title: string; subtitle?: ReactNode; actions?: ReactNode }) {
  return (
    <Stack direction="row" sx={{ alignItems: "flex-end", justifyContent: "space-between", mb: 3, gap: 2, flexWrap: "wrap" }}>
      <Box>
        <Typography variant="h4" component="h1">
          {title}
        </Typography>
        {subtitle && (
          <Typography variant="body2" color="text.secondary" sx={{ mt: 0.5 }}>
            {subtitle}
          </Typography>
        )}
      </Box>
      {actions && <Stack direction="row" sx={{ gap: 1 }}>{actions}</Stack>}
    </Stack>
  );
}

export const phaseLabels: Record<SessionPhase, string> = {
  idle: "Ready",
  starting: "Starting",
  streaming: "Streaming",
  client_disconnected: "Client disconnected",
  reconnecting: "Reconnecting",
  stopping: "Ending session",
  error: "Error",
};

export const phaseColors: Record<SessionPhase, string> = {
  idle: "text.secondary",
  starting: "info.main",
  streaming: "success.main",
  client_disconnected: "warning.main",
  reconnecting: "info.main",
  stopping: "text.secondary",
  error: "error.main",
};

export function StatusDot({ phase, size = 12 }: { phase: SessionPhase; size?: number }) {
  const live = phase === "streaming" || phase === "starting" || phase === "reconnecting";
  return (
    <Box
      component="span"
      sx={{
        display: "inline-block",
        width: size,
        height: size,
        borderRadius: "50%",
        bgcolor: phaseColors[phase],
        boxShadow: live ? (theme) => `0 0 0 4px color-mix(in srgb, ${theme.vars?.palette.success.main ?? "green"} 20%, transparent)` : "none",
        animation: phase === "starting" || phase === "reconnecting" ? "pulse 1.4s ease-in-out infinite" : "none",
        "@keyframes pulse": { "50%": { opacity: 0.35 } },
        flexShrink: 0,
      }}
    />
  );
}

/// A command the user can copy (service management stays with the user).
export function CopyableCommand({ command }: { command: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Stack
      direction="row"
      sx={{
        alignItems: "center",
        gap: 1,
        bgcolor: "action.hover",
        borderRadius: 2,
        pl: 1.5,
        pr: 0.5,
        py: 0.25,
        fontFamily: "monospace",
        fontSize: 13,
      }}
    >
      <Box component="code" sx={{ flexGrow: 1, overflowX: "auto", whiteSpace: "nowrap" }}>
        {command}
      </Box>
      <Tooltip title={copied ? "Copied" : "Copy"}>
        <IconButton
          size="small"
          aria-label="Copy command"
          onClick={() => {
            void navigator.clipboard?.writeText(command).then(() => {
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            });
          }}
        >
          {copied ? <Check fontSize="small" /> : <ContentCopy fontSize="small" />}
        </IconButton>
      </Tooltip>
    </Stack>
  );
}

export function ConfirmDialog({
  open,
  title,
  children,
  confirm,
  destructive,
  busy,
  onConfirm,
  onClose,
}: {
  open: boolean;
  title: string;
  children: ReactNode;
  confirm: string;
  destructive?: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onClose: () => void;
}) {
  return (
    <Dialog open={open} onClose={busy ? undefined : onClose} maxWidth="xs" fullWidth>
      <DialogTitle>{title}</DialogTitle>
      <DialogContent>
        <DialogContentText component="div">{children}</DialogContentText>
      </DialogContent>
      <DialogActions sx={{ px: 3, pb: 2 }}>
        <Button onClick={onClose} disabled={busy}>
          Cancel
        </Button>
        <Button variant="contained" color={destructive ? "error" : "primary"} onClick={onConfirm} disabled={busy}>
          {confirm}
        </Button>
      </DialogActions>
    </Dialog>
  );
}

/// Label/value pairs in two columns.
export function Facts({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <Box component="dl" sx={{ display: "grid", gridTemplateColumns: "minmax(140px, max-content) 1fr", columnGap: 3, rowGap: 1.25, m: 0 }}>
      {rows.map(([label, value]) => (
        <Box key={label} sx={{ display: "contents" }}>
          <Typography component="dt" variant="body2" color="text.secondary">
            {label}
          </Typography>
          <Typography component="dd" variant="body2" sx={{ m: 0, wordBreak: "break-word" }}>
            {value}
          </Typography>
        </Box>
      ))}
    </Box>
  );
}
