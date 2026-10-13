// FlowLite: sync settings and the all-devices history. Kept out of upstream Handy files.
import React, { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { Copy } from "lucide-react";
import { toast } from "sonner";
import "./branding";
import { SettingsGroup } from "../ui/SettingsGroup";
import { Button } from "../ui/Button";
import { Input } from "../ui/Input";
import Badge from "../ui/Badge";
import { copyToClipboard } from "../settings/history/clipboard";
import { formatDateTime } from "@/utils/dateFormat";

interface SyncStatus {
  server_url: string;
  email: string;
  signed_in: boolean;
  message: string;
  update_ready: boolean;
}

interface SyncedTranscript {
  id: string;
  text: string;
  device: string;
  app_name: string | null;
  model: string | null;
  transcribe_ms: number | null;
  created_at: number;
}

export const FlowLiteSync: React.FC = () => {
  const { t, i18n } = useTranslation();
  const [status, setStatus] = useState<SyncStatus | null>(null);
  const [server, setServer] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [query, setQuery] = useState("");
  const [items, setItems] = useState<SyncedTranscript[]>([]);

  const refresh = useCallback(async () => {
    const s = await invoke<SyncStatus>("flowlite_status");
    setStatus(s);
    setServer((v) => v || s.server_url);
    setEmail((v) => v || s.email);
  }, []);

  const loadItems = useCallback(async () => {
    setItems(
      await invoke<SyncedTranscript[]>("flowlite_transcripts", { query }),
    );
  }, [query]);

  useEffect(() => {
    refresh();
    const un = listen("flowlite-sync-status", () => {
      refresh();
      loadItems();
    });
    return () => {
      un.then((f) => f());
    };
  }, [refresh, loadItems]);

  useEffect(() => {
    loadItems();
  }, [loadItems]);

  const run = async (fn: () => Promise<SyncStatus>) => {
    setBusy(true);
    try {
      setStatus(await fn());
      await loadItems();
    } catch (e) {
      toast.error(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup
        title={t("flowlite.sync.title", "Sync")}
        description={t(
          "flowlite.sync.description",
          "Shares transcripts and your dictionary with FlowLite on Android. Audio never leaves this computer.",
        )}
      >
        <div className="p-4 space-y-3">
          <Input
            value={server}
            onChange={(e) => setServer(e.target.value)}
            placeholder={t("flowlite.sync.server", "Server URL")}
            disabled={status?.signed_in}
          />
          {status?.signed_in ? (
            <>
              <p className="text-sm">
                {t("flowlite.sync.signedInAs", "Signed in as {{email}}", {
                  email: status.email,
                })}
              </p>
              <div className="flex gap-2">
                <Button
                  size="sm"
                  disabled={busy}
                  onClick={() => run(() => invoke("flowlite_sync_now"))}
                >
                  {t("flowlite.sync.now", "Sync now")}
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={busy}
                  onClick={() => run(() => invoke("flowlite_logout"))}
                >
                  {t("flowlite.sync.signOut", "Sign out")}
                </Button>
              </div>
            </>
          ) : (
            <>
              <Input
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                placeholder={t("flowlite.sync.email", "Email")}
              />
              <Input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder={t("flowlite.sync.password", "Password")}
              />
              <Button
                size="sm"
                disabled={busy || !email || !password}
                onClick={() =>
                  run(async () => {
                    const s = await invoke<SyncStatus>("flowlite_login", {
                      serverUrl: server,
                      email,
                      password,
                    });
                    setPassword("");
                    return s;
                  })
                }
              >
                {t("flowlite.sync.signIn", "Sign in")}
              </Button>
            </>
          )}
          <p className="text-xs text-mid-gray">{status?.message}</p>
          {status?.update_ready && (
            <div className="flex items-center gap-2">
              <p className="text-sm flex-1">
                {t(
                  "flowlite.update.ready",
                  "A new version of FlowLite is ready.",
                )}
              </p>
              <Button
                size="sm"
                disabled={busy}
                onClick={async () => {
                  setBusy(true);
                  try {
                    await invoke("flowlite_install_update");
                  } catch (e) {
                    toast.error(String(e));
                  } finally {
                    setBusy(false);
                  }
                }}
              >
                {t("flowlite.update.install", "Install update")}
              </Button>
            </div>
          )}
        </div>
      </SettingsGroup>

      <SettingsGroup
        title={t("flowlite.history.title", "All devices")}
        description={t(
          "flowlite.history.description",
          "Transcripts from this computer and your phone.",
        )}
      >
        <div className="p-4 space-y-3">
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("flowlite.history.search", "Search")}
          />
          {items.length === 0 && (
            <p className="text-sm text-mid-gray">
              {t("flowlite.history.empty", "No transcripts yet.")}
            </p>
          )}
          {items.map((it) => (
            <div
              key={it.id}
              className="border border-mid-gray/20 rounded-md p-3 space-y-2"
            >
              <p className="text-sm select-text whitespace-pre-wrap">
                {it.text}
              </p>
              <div className="flex items-center gap-2 text-xs text-mid-gray">
                <Badge
                  variant={it.device === "android" ? "success" : "secondary"}
                >
                  {it.device === "android"
                    ? t("flowlite.device.android", "Android")
                    : t("flowlite.device.linux", "Linux")}
                </Badge>
                <span className="flex-1">
                  {formatDateTime(
                    String(Math.floor(it.created_at / 1000)),
                    i18n.language,
                  )}
                  {it.transcribe_ms
                    ? ` · ${(it.transcribe_ms / 1000).toFixed(1)} s`
                    : ""}
                  {it.model ? ` · ${it.model}` : ""}
                </span>
                <button
                  className="p-1.5 rounded-md text-text/50 hover:text-logo-primary cursor-pointer"
                  title={t("flowlite.history.copy", "Copy")}
                  onClick={async () => {
                    await copyToClipboard(it.text);
                    toast.success(t("flowlite.history.copied", "Copied"));
                  }}
                >
                  <Copy className="w-4 h-4" />
                </button>
              </div>
            </div>
          ))}
        </div>
      </SettingsGroup>
    </div>
  );
};
