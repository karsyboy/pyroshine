import { useState } from "react";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import CircularProgress from "@mui/material/CircularProgress";
import Stack from "@mui/material/Stack";
import TextField from "@mui/material/TextField";
import Typography from "@mui/material/Typography";
import { errorKind, errorMessage } from "../api/client";
import type { AuthState } from "../api/types";
import { CopyableCommand } from "../components/common";
import { useAuth } from "../state/auth";

function Frame({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <Box sx={{ minHeight: "100vh", display: "grid", placeItems: "center", bgcolor: "background.default", p: 2 }}>
      <Card sx={{ width: "100%", maxWidth: 480 }}>
        <CardContent sx={{ p: { xs: 3, sm: 4 } }}>
          <Stack sx={{ gap: 2.5 }}>
            <Stack direction="row" sx={{ gap: 1.5, alignItems: "center" }}>
              <Box component="img" src="/logo.png" alt="" sx={{ width: 40, height: 40 }} />
              <Typography variant="h5" component="h1">
                {title}
              </Typography>
            </Stack>
            {children}
          </Stack>
        </CardContent>
      </Card>
    </Box>
  );
}

/// First run: the operator proves control of the streaming account with a
/// one-time code from the host, then chooses the administrator password.
function Setup({ state, reset }: { state: AuthState; reset: boolean }) {
  const auth = useAuth();
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const min = state.min_password_length;
  const mismatch = confirm.length > 0 && confirm !== password;
  const valid = code.trim().length >= 16 && password.length >= min && password === confirm;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await auth.setup(code, password);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Frame title={reset ? "Reset the Pyroshine password" : "Set up Pyroshine"}>
      <Typography color="text.secondary">
        {reset
          ? "Enter a reset code to choose a new administrator password. Every signed-in browser is signed out."
          : "Choose the administrator password for this web interface. To prove you control the host, enter a setup code from it."}
      </Typography>
      <Typography variant="body2" color="text.secondary">
        Run this on the host as the streaming user (or over SSH). The code is valid for 15 minutes:
      </Typography>
      <CopyableCommand command={reset ? "pyroshine web setup --reset" : "pyroshine web setup"} />
      <Typography variant="body2" color="text.secondary">
        On a desktop, the Pyroshine tray's <b>Set Up Web Access</b> also shows a code.
      </Typography>
      <Box
        component="form"
        onSubmit={(event) => {
          event.preventDefault();
          if (valid && !busy) void submit();
        }}
      >
        <Stack sx={{ gap: 2 }}>
          <TextField
            label="Setup code"
            value={code}
            autoFocus
            autoComplete="one-time-code"
            onChange={(event) => setCode(event.target.value.toUpperCase())}
            slotProps={{ htmlInput: { spellCheck: false, autoCapitalize: "characters", style: { fontFamily: "monospace" } } }}
            placeholder="XXXX-XXXX-XXXX-XXXX"
          />
          <TextField
            label="New password"
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            helperText={`At least ${min} characters. A passphrase is easiest to remember.`}
          />
          <TextField
            label="Repeat the password"
            type="password"
            autoComplete="new-password"
            value={confirm}
            error={mismatch}
            helperText={mismatch ? "The passwords differ." : " "}
            onChange={(event) => setConfirm(event.target.value)}
          />
          {error && <Alert severity="error">{error}</Alert>}
          <Button type="submit" variant="contained" disabled={!valid || busy} startIcon={busy ? <CircularProgress size={16} /> : undefined}>
            {reset ? "Set new password" : "Set password and sign in"}
          </Button>
        </Stack>
      </Box>
    </Frame>
  );
}

function Login({ reason }: { reason: string | null }) {
  const auth = useAuth();
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<{ kind: string; message: string } | null>(null);
  const [forgot, setForgot] = useState(false);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await auth.login(password);
    } catch (e) {
      setError({ kind: errorKind(e), message: errorMessage(e) });
      setPassword("");
    } finally {
      setBusy(false);
    }
  };

  if (forgot && auth.state) return <Setup state={auth.state} reset />;
  return (
    <Frame title="Sign in to Pyroshine">
      {reason && <Alert severity="info">{reason}</Alert>}
      <Box
        component="form"
        onSubmit={(event) => {
          event.preventDefault();
          if (password && !busy) void submit();
        }}
      >
        <Stack sx={{ gap: 2 }}>
          <TextField
            label="Administrator password"
            type="password"
            autoComplete="current-password"
            autoFocus
            value={password}
            onChange={(event) => setPassword(event.target.value)}
          />
          {error && <Alert severity={error.kind === "rate_limited" ? "warning" : "error"}>{error.message}</Alert>}
          <Button type="submit" variant="contained" disabled={!password || busy} startIcon={busy ? <CircularProgress size={16} /> : undefined}>
            Sign in
          </Button>
          <Button size="small" onClick={() => setForgot(true)} sx={{ alignSelf: "flex-start" }}>
            Forgot the password?
          </Button>
        </Stack>
      </Box>
    </Frame>
  );
}

/// What a browser sees before it is signed in.
export function SignInPage() {
  const auth = useAuth();
  if (!auth.state) {
    return (
      <Frame title="Pyroshine">
        {auth.error ? (
          <>
            <Alert severity="warning">{auth.error}</Alert>
            <Typography variant="body2" color="text.secondary">
              Retrying… On the host, check the service with <code>systemctl status "pyroshine@$USER"</code>.
            </Typography>
          </>
        ) : (
          <Stack sx={{ alignItems: "center", py: 2 }}>
            <CircularProgress aria-label="Connecting to Pyroshine" />
          </Stack>
        )}
      </Frame>
    );
  }
  if (auth.state.locked) {
    return (
      <>
        <Box sx={{ position: "fixed", top: 16, left: 16, right: 16, zIndex: 1 }}>
          <Alert severity="error" sx={{ maxWidth: 720, mx: "auto" }}>
            The stored sign-in data is damaged ({auth.state.locked}). Reset the password to continue.
          </Alert>
        </Box>
        <Setup state={auth.state} reset />
      </>
    );
  }
  if (!auth.state.configured) return <Setup state={auth.state} reset={false} />;
  return <Login reason={auth.signedOutReason} />;
}
