import React, { useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import { Activity, Database, RefreshCw, Server, Shield } from "lucide-react";
import "./styles.css";

type StatsResponse = {
  summary: {
    total: number;
    active: number;
    cooldown: number;
    disabled: number;
    rpm: number;
    total_input_tokens: number;
    total_output_tokens: number;
    total_cached_tokens: number;
    total_reasoning_tokens: number;
  };
  accounts: Array<{
    file_path: string;
    email: string;
    status: string;
    used_percent: number;
    successful_requests: number;
    failed_requests: number;
    attempt_requests: number;
    attempt_errors: number;
    last_used_at?: string;
    cooldown_until?: string;
    quota_exhausted: boolean;
    usage: {
      total_completions: number;
      input_tokens: number;
      output_tokens: number;
      cached_tokens: number;
      reasoning_tokens: number;
      total_tokens: number;
    };
  }>;
};

type RequestLog = {
  id: number;
  ts_ms: number;
  endpoint: string;
  model: string;
  stream: boolean;
  status: number;
  attempts: number;
  api_key?: string;
  account_file_path?: string;
  error_message?: string;
  duration_ms: number;
};

type UsageLog = {
  id: number;
  ts_ms: number;
  endpoint: string;
  model: string;
  account_file_path: string;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  reasoning_tokens: number;
  total_tokens: number;
};

type RateLimits = {
  key_rpm: number;
  key_concurrency: number;
  account_rpm: number;
  account_concurrency: number;
  image_concurrency: number;
};

async function fetchJson<T>(path: string): Promise<T> {
  const res = await fetch(path);
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`);
  }
  return res.json() as Promise<T>;
}

function App() {
  const [stats, setStats] = useState<StatsResponse | null>(null);
  const [requests, setRequests] = useState<RequestLog[]>([]);
  const [usage, setUsage] = useState<UsageLog[]>([]);
  const [limits, setLimits] = useState<RateLimits | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function load() {
    setLoading(true);
    setError(null);
    try {
      const [statsData, requestData, usageData, limitData] = await Promise.all([
        fetchJson<StatsResponse>("/stats"),
        fetchJson<{ data: RequestLog[] }>("/admin/request-logs?limit=80").catch(() => ({ data: [] })),
        fetchJson<{ data: UsageLog[] }>("/admin/usage-logs?limit=80").catch(() => ({ data: [] })),
        fetchJson<{ data: RateLimits | null }>("/admin/rate-limits").catch(() => ({ data: null }))
      ]);
      setStats(statsData);
      setRequests(requestData.data);
      setUsage(usageData.data);
      setLimits(limitData.data);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    load();
    const timer = window.setInterval(load, 15000);
    return () => window.clearInterval(timer);
  }, []);

  const totals = useMemo(() => {
    if (!stats) return [];
    return [
      ["Accounts", stats.summary.total],
      ["Active", stats.summary.active],
      ["Cooldown", stats.summary.cooldown],
      ["RPM", stats.summary.rpm],
      ["Input", stats.summary.total_input_tokens],
      ["Output", stats.summary.total_output_tokens]
    ];
  }, [stats]);

  return (
    <main>
      <header className="topbar">
        <div>
          <h1>codex-proxy-rs</h1>
          <p>Accounts, traffic, usage, and runtime limits</p>
        </div>
        <button onClick={load} disabled={loading}>
          <RefreshCw size={16} />
          Refresh
        </button>
      </header>

      {error && <div className="alert">{error}</div>}

      <section className="metrics">
        {totals.map(([label, value]) => (
          <div className="metric" key={label}>
            <span>{label}</span>
            <strong>{value.toLocaleString()}</strong>
          </div>
        ))}
      </section>

      <section className="grid">
        <Panel icon={<Server size={18} />} title="Accounts">
          <table>
            <thead>
              <tr>
                <th>Email</th>
                <th>Status</th>
                <th>Quota</th>
                <th>Requests</th>
                <th>Tokens</th>
              </tr>
            </thead>
            <tbody>
              {(stats?.accounts ?? []).map((account) => (
                <tr key={account.file_path}>
                  <td>{account.email || account.file_path.split("/").pop()}</td>
                  <td><Status value={account.status} /></td>
                  <td>{account.used_percent >= 0 ? `${account.used_percent.toFixed(1)}%` : "-"}</td>
                  <td>{account.successful_requests}/{account.attempt_requests}</td>
                  <td>{account.usage.total_tokens.toLocaleString()}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </Panel>

        <Panel icon={<Shield size={18} />} title="Limits">
          <div className="limit-list">
            <Limit label="Key RPM" value={limits?.key_rpm} />
            <Limit label="Key Concurrency" value={limits?.key_concurrency} />
            <Limit label="Account RPM" value={limits?.account_rpm} />
            <Limit label="Account Concurrency" value={limits?.account_concurrency} />
            <Limit label="Image Concurrency" value={limits?.image_concurrency} />
          </div>
        </Panel>

        <Panel icon={<Activity size={18} />} title="Request Logs">
          <LogTable rows={requests} />
        </Panel>

        <Panel icon={<Database size={18} />} title="Usage Logs">
          <table>
            <thead>
              <tr>
                <th>Time</th>
                <th>Endpoint</th>
                <th>Model</th>
                <th>Total</th>
              </tr>
            </thead>
            <tbody>
              {usage.map((row) => (
                <tr key={row.id}>
                  <td>{formatTime(row.ts_ms)}</td>
                  <td>{row.endpoint}</td>
                  <td>{row.model}</td>
                  <td>{row.total_tokens.toLocaleString()}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </Panel>
      </section>
    </main>
  );
}

function Panel(props: { icon: React.ReactNode; title: string; children: React.ReactNode }) {
  return (
    <section className="panel">
      <h2>{props.icon}{props.title}</h2>
      {props.children}
    </section>
  );
}

function Status({ value }: { value: string }) {
  return <span className={`status ${value}`}>{value}</span>;
}

function Limit({ label, value }: { label: string; value?: number }) {
  return (
    <div className="limit">
      <span>{label}</span>
      <strong>{value && value > 0 ? value : "off"}</strong>
    </div>
  );
}

function LogTable({ rows }: { rows: RequestLog[] }) {
  return (
    <table>
      <thead>
        <tr>
          <th>Time</th>
          <th>Endpoint</th>
          <th>Status</th>
          <th>Model</th>
          <th>ms</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((row) => (
          <tr key={row.id}>
            <td>{formatTime(row.ts_ms)}</td>
            <td>{row.endpoint}</td>
            <td>{row.status}</td>
            <td>{row.model}</td>
            <td>{row.duration_ms}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function formatTime(ms: number) {
  if (!ms) return "-";
  return new Date(ms).toLocaleTimeString();
}

createRoot(document.getElementById("root")!).render(<App />);
