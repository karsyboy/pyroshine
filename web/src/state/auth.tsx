import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { api, errorMessage } from "../api/client";
import { setSignedOutHandler } from "../api/http";
import type { AuthState } from "../api/types";

/// Sign-in state of this browser. The session itself is an HttpOnly cookie;
/// the page only learns whether it is signed in and its CSRF token.
interface Auth {
  /** `null` until the first answer. */
  state: AuthState | null;
  /** Why the sign-in state could not be read (server unreachable). */
  error: string | null;
  /** Set after the server ended this browser's session. */
  signedOutReason: string | null;
  reload: () => Promise<void>;
  setup: (code: string, password: string) => Promise<void>;
  login: (password: string) => Promise<void>;
  logout: () => Promise<void>;
  /** The server reported the session as ended. */
  signedOut: (reason?: string) => void;
}

const Context = createContext<Auth | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AuthState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [signedOutReason, setSignedOutReason] = useState<string | null>(null);

  const reload = useCallback(async () => {
    try {
      setState(await api.auth());
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);

  const signedOut = useCallback((reason = "Your session ended. Sign in again.") => {
    setSignedOutReason(reason);
    setState((current) => (current ? { ...current, authenticated: false, csrf_token: null, session: null } : current));
  }, []);

  useEffect(() => {
    setSignedOutHandler(() => signedOut());
    void reload();
    return () => setSignedOutHandler(null);
  }, [reload, signedOut]);

  // Retry while the server cannot be reached.
  useEffect(() => {
    if (!error) return;
    const timer = setTimeout(() => void reload(), 3000);
    return () => clearTimeout(timer);
  }, [error, reload]);

  const value = useMemo<Auth>(
    () => ({
      state,
      error,
      signedOutReason,
      reload,
      setup: async (code, password) => {
        setState(await api.setup(code, password));
        setSignedOutReason(null);
      },
      login: async (password) => {
        setState(await api.login(password));
        setSignedOutReason(null);
      },
      logout: async () => {
        try {
          await api.logout();
        } finally {
          setSignedOutReason(null);
          await reload();
        }
      },
      signedOut,
    }),
    [state, error, signedOutReason, reload, signedOut],
  );
  return <Context.Provider value={value}>{children}</Context.Provider>;
}

export function useAuth(): Auth {
  const value = useContext(Context);
  if (!value) throw new Error("useAuth outside AuthProvider");
  return value;
}
