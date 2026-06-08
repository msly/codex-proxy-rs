import React, { useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Alert,
  Button,
  Card,
  ConfigProvider,
  Empty,
  Form,
  Input,
  Layout,
  Menu,
  Space,
  Statistic,
  Table,
  Tag,
  Typography,
  message,
  theme
} from "antd";
import type { ColumnsType } from "antd/es/table";
import {
  ApiOutlined,
  BarChartOutlined,
  ClockCircleOutlined,
  DashboardOutlined,
  LockOutlined,
  LogoutOutlined,
  ReloadOutlined,
  SafetyOutlined,
  SettingOutlined,
  TeamOutlined,
  UserOutlined
} from "@ant-design/icons";
import "antd/dist/reset.css";
import "./styles.css";

type PageKey = "overview" | "accounts" | "requests" | "usage" | "runtime" | "settings";

type AdminStatus = {
  username: string;
  initialized: boolean;
};

type AuthResponse = {
  data: {
    token: string;
    status: AdminStatus;
  };
};

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
  accounts: AccountRow[];
};

type AccountRow = {
  file_path: string;
  email: string;
  status: string;
  used_percent: number;
  successful_requests: number;
  failed_requests: number;
  attempt_requests: number;
  attempt_errors: number;
  consecutive_failures?: number;
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
  error_type?: string;
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

type PersistenceStatus = {
  enabled: boolean;
  writer_running: boolean;
  dropped_events: number;
  write_errors: number;
};

type ApiErrorBody = {
  error?: {
    message?: string;
    type?: string;
  };
};

const ADMIN_TOKEN_STORAGE = "codex_proxy_rs_admin_token";

async function fetchJson<T>(path: string, token?: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers);
  if (token?.trim()) {
    headers.set("Authorization", `Bearer ${token.trim()}`);
  }
  const res = await fetch(path, { ...init, headers });
  if (!res.ok) {
    let detail = `${res.status} ${res.statusText}`;
    try {
      const body = (await res.json()) as ApiErrorBody;
      if (body.error?.message) {
        detail = `${detail}: ${body.error.message}`;
      }
    } catch {
      // Keep the HTTP status when the backend does not return JSON.
    }
    throw new Error(detail);
  }
  return res.json() as Promise<T>;
}

function App() {
  const [messageApi, contextHolder] = message.useMessage();
  const [page, setPage] = useState<PageKey>("overview");
  const [adminStatus, setAdminStatus] = useState<AdminStatus | null>(null);
  const [adminToken, setAdminToken] = useState(() => window.sessionStorage.getItem(ADMIN_TOKEN_STORAGE) ?? "");
  const [authLoading, setAuthLoading] = useState(true);
  const [authError, setAuthError] = useState<string | null>(null);
  const [stats, setStats] = useState<StatsResponse | null>(null);
  const [requests, setRequests] = useState<RequestLog[]>([]);
  const [usage, setUsage] = useState<UsageLog[]>([]);
  const [limits, setLimits] = useState<RateLimits | null>(null);
  const [persistence, setPersistence] = useState<PersistenceStatus | null>(null);
  const [loading, setLoading] = useState(false);
  const [statsError, setStatsError] = useState<string | null>(null);
  const [requestError, setRequestError] = useState<string | null>(null);
  const [usageError, setUsageError] = useState<string | null>(null);
  const [runtimeError, setRuntimeError] = useState<string | null>(null);

  const appTheme = {
    algorithm: theme.defaultAlgorithm,
    token: {
      borderRadius: 6,
      colorPrimary: "#1677ff",
      fontFamily:
        'Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif'
    }
  };

  useEffect(() => {
    void loadAdminStatus();
  }, []);

  useEffect(() => {
    if (!adminToken) return;
    void load();
    const timer = window.setInterval(() => void load(), 15000);
    return () => window.clearInterval(timer);
  }, [adminToken]);

  async function loadAdminStatus() {
    setAuthLoading(true);
    setAuthError(null);
    try {
      const res = await fetchJson<{ data: AdminStatus }>("/admin/status");
      setAdminStatus(res.data);
    } catch (err) {
      setAuthError(normalizeError(err));
    } finally {
      setAuthLoading(false);
    }
  }

  async function login(values: { username: string; password: string }) {
    setAuthLoading(true);
    setAuthError(null);
    try {
      const res = await postJson<AuthResponse>("/admin/login", values);
      setAdminStatus(res.data.status);
      setAdminToken(res.data.token);
      window.sessionStorage.setItem(ADMIN_TOKEN_STORAGE, res.data.token);
    } catch (err) {
      setAuthError(normalizeError(err));
    } finally {
      setAuthLoading(false);
    }
  }

  async function setup(values: { username: string; password: string }) {
    setAuthLoading(true);
    setAuthError(null);
    try {
      const res = await postJson<AuthResponse>("/admin/setup", values);
      setAdminStatus(res.data.status);
      setAdminToken(res.data.token);
      window.sessionStorage.setItem(ADMIN_TOKEN_STORAGE, res.data.token);
    } catch (err) {
      setAuthError(normalizeError(err));
    } finally {
      setAuthLoading(false);
    }
  }

  async function logout() {
    const token = adminToken;
    window.sessionStorage.removeItem(ADMIN_TOKEN_STORAGE);
    setAdminToken("");
    setStats(null);
    setRequests([]);
    setUsage([]);
    setLimits(null);
    setPersistence(null);
    if (token) {
      try {
        await fetchJson("/admin/logout", token, { method: "POST" });
      } catch {
        // Logging out locally is enough if the server session has already expired.
      }
    }
  }

  async function load() {
    if (!adminToken) return;

    setLoading(true);
    setStatsError(null);
    setRequestError(null);
    setUsageError(null);
    setRuntimeError(null);

    const [statsResult, requestResult, usageResult, limitResult, persistenceResult] =
      await Promise.allSettled([
        fetchJson<StatsResponse>("/stats", adminToken),
        fetchJson<{ data: RequestLog[] }>("/admin/request-logs?limit=200", adminToken),
        fetchJson<{ data: UsageLog[] }>("/admin/usage-logs?limit=200", adminToken),
        fetchJson<{ data: RateLimits | null }>("/admin/rate-limits", adminToken),
        fetchJson<{ data: PersistenceStatus | null }>("/admin/persistence", adminToken)
      ]);

    if (statsResult.status === "fulfilled") {
      setStats(statsResult.value);
    } else {
      const messageText = normalizeError(statsResult.reason);
      setStatsError(messageText);
      if (isAuthError(messageText)) {
        await logout();
        setAuthError(messageText);
      }
    }

    if (requestResult.status === "fulfilled") {
      setRequests(requestResult.value.data);
    } else {
      setRequests([]);
      setRequestError(normalizeError(requestResult.reason));
    }

    if (usageResult.status === "fulfilled") {
      setUsage(usageResult.value.data);
    } else {
      setUsage([]);
      setUsageError(normalizeError(usageResult.reason));
    }

    if (limitResult.status === "fulfilled") {
      setLimits(limitResult.value.data);
    } else {
      setLimits(null);
      setRuntimeError(normalizeError(limitResult.reason));
    }

    if (persistenceResult.status === "fulfilled") {
      setPersistence(persistenceResult.value.data);
    } else {
      setPersistence(null);
      setRuntimeError((prev) => [prev, normalizeError(persistenceResult.reason)].filter(Boolean).join("; "));
    }

    setLoading(false);
  }

  async function changePassword(values: { current_password: string; new_password: string }) {
    await postJson("/admin/change-password", values, adminToken);
    messageApi.success("Password updated. Please log in again.");
    await logout();
  }

  const title = pageTitles[page];
  const subtitle = pageSubtitles[page];

  if (!adminToken) {
    return (
      <ConfigProvider theme={appTheme}>
        {contextHolder}
        <LoginPage
          adminStatus={adminStatus}
          error={authError}
          loading={authLoading}
          onLogin={login}
          onSetup={setup}
        />
      </ConfigProvider>
    );
  }

  return (
    <ConfigProvider theme={appTheme}>
      {contextHolder}
      <Layout className="app-shell">
        <Layout.Sider className="app-sider" breakpoint="lg" collapsedWidth={0} width={232}>
          <div className="brand">
            <ApiOutlined />
            <span>codex-proxy-rs</span>
          </div>
          <Menu
            mode="inline"
            selectedKeys={[page]}
            onClick={({ key }) => setPage(key as PageKey)}
            items={[
              { key: "overview", icon: <DashboardOutlined />, label: "Overview" },
              { key: "accounts", icon: <TeamOutlined />, label: "Accounts" },
              { key: "requests", icon: <ClockCircleOutlined />, label: "Request Logs" },
              { key: "usage", icon: <BarChartOutlined />, label: "Usage Logs" },
              { key: "runtime", icon: <SafetyOutlined />, label: "Runtime" },
              { key: "settings", icon: <SettingOutlined />, label: "Settings" }
            ]}
          />
        </Layout.Sider>

        <Layout>
          <Layout.Header className="app-header">
            <div>
              <Typography.Title level={3}>{title}</Typography.Title>
              <Typography.Text type="secondary">{subtitle}</Typography.Text>
            </div>
            <Space wrap>
              <Button icon={<ReloadOutlined spin={loading} />} loading={loading} onClick={() => void load()}>
                Refresh
              </Button>
              <Button icon={<LogoutOutlined />} onClick={() => void logout()}>
                Logout
              </Button>
            </Space>
          </Layout.Header>

          <Layout.Content className="app-content">
            {statsError && (
              <Alert className="page-alert" message="Stats unavailable" description={statsError} type="error" showIcon />
            )}
            {page === "overview" && <OverviewPage stats={stats} loading={loading} />}
            {page === "accounts" && <AccountsPage stats={stats} loading={loading} />}
            {page === "requests" && (
              <RequestLogsPage
                loading={loading}
                persistence={persistence}
                requestError={requestError}
                rows={requests}
              />
            )}
            {page === "usage" && (
              <UsageLogsPage loading={loading} persistence={persistence} rows={usage} usageError={usageError} />
            )}
            {page === "runtime" && (
              <RuntimePage
                limits={limits}
                loading={loading}
                persistence={persistence}
                runtimeError={runtimeError}
              />
            )}
            {page === "settings" && (
              <SettingsPage adminStatus={adminStatus} loading={authLoading} onChangePassword={changePassword} />
            )}
          </Layout.Content>
        </Layout>
      </Layout>
    </ConfigProvider>
  );
}

async function postJson<T>(path: string, body: unknown, token?: string): Promise<T> {
  return fetchJson<T>(path, token, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body)
  });
}

function LoginPage({
  adminStatus,
  error,
  loading,
  onLogin,
  onSetup
}: {
  adminStatus: AdminStatus | null;
  error: string | null;
  loading: boolean;
  onLogin: (values: { username: string; password: string }) => Promise<void>;
  onSetup: (values: { username: string; password: string }) => Promise<void>;
}) {
  const initialized = adminStatus?.initialized ?? true;
  const [form] = Form.useForm();

  useEffect(() => {
    if (adminStatus?.username) {
      form.setFieldValue("username", adminStatus.username);
    }
  }, [adminStatus?.username, form]);

  return (
    <main className="login-shell">
      <Card className="login-card">
        <Space className="login-stack" direction="vertical" size={20}>
          <div>
            <div className="login-brand">
              <ApiOutlined />
              <span>codex-proxy-rs</span>
            </div>
            <Typography.Title level={3}>{initialized ? "Admin Login" : "Initialize Admin"}</Typography.Title>
            <Typography.Text type="secondary">
              {initialized
                ? "Sign in with your admin username and password."
                : "Create the first admin account before opening the dashboard."}
            </Typography.Text>
          </div>

          {error && <Alert message={initialized ? "Login failed" : "Setup failed"} description={error} type="error" showIcon />}

          <Form
            form={form}
            layout="vertical"
            onFinish={(values) => {
              if (initialized) {
                void onLogin(values as { username: string; password: string });
              } else {
                void onSetup(values as { username: string; password: string });
              }
            }}
          >
            <Form.Item
              label="Username"
              name="username"
              rules={[{ required: true, message: "Username is required" }]}
            >
              <Input autoFocus prefix={<UserOutlined />} placeholder="admin" />
            </Form.Item>
            <Form.Item
              label="Password"
              name="password"
              rules={[{ required: true, message: "Password is required" }]}
            >
              <Input.Password prefix={<LockOutlined />} placeholder="Password" />
            </Form.Item>
            <Button block htmlType="submit" loading={loading} type="primary">
              {initialized ? "Login" : "Create Admin"}
            </Button>
          </Form>
        </Space>
      </Card>
    </main>
  );
}

function OverviewPage({ stats, loading }: { stats: StatsResponse | null; loading: boolean }) {
  const totals = useMemo(
    () => [
      ["Accounts", stats?.summary.total],
      ["Active", stats?.summary.active],
      ["Cooldown", stats?.summary.cooldown],
      ["Disabled", stats?.summary.disabled],
      ["RPM", stats?.summary.rpm],
      ["Input Tokens", stats?.summary.total_input_tokens],
      ["Output Tokens", stats?.summary.total_output_tokens],
      ["Cached Tokens", stats?.summary.total_cached_tokens],
      ["Reasoning Tokens", stats?.summary.total_reasoning_tokens]
    ],
    [stats]
  );

  return (
    <Space className="page-stack" direction="vertical" size={16}>
      <div className="metrics-grid">
        {totals.map(([label, value]) => (
          <Card key={label} loading={loading && !stats}>
            <Statistic title={label} value={typeof value === "number" ? value : 0} />
          </Card>
        ))}
      </div>
      <Card title="Account Snapshot">
        <AccountsTable accounts={stats?.accounts ?? []} compact loading={loading} />
      </Card>
    </Space>
  );
}

function AccountsPage({ stats, loading }: { stats: StatsResponse | null; loading: boolean }) {
  return (
    <Card title="Accounts">
      <AccountsTable accounts={stats?.accounts ?? []} loading={loading} />
    </Card>
  );
}

function AccountsTable({
  accounts,
  compact = false,
  loading
}: {
  accounts: AccountRow[];
  compact?: boolean;
  loading: boolean;
}) {
  const columns: ColumnsType<AccountRow> = [
    {
      title: "Email",
      dataIndex: "email",
      sorter: (a, b) => displayAccount(a).localeCompare(displayAccount(b)),
      render: (_, row) => (
        <Typography.Text ellipsis title={displayAccount(row)}>
          {displayAccount(row)}
        </Typography.Text>
      )
    },
    {
      title: "Status",
      dataIndex: "status",
      width: 130,
      filters: [
        { text: "active", value: "active" },
        { text: "cooldown", value: "cooldown" },
        { text: "disabled", value: "disabled" }
      ],
      onFilter: (value, row) => row.status === value,
      sorter: (a, b) => a.status.localeCompare(b.status),
      render: (value: string) => <StatusTag value={value} />
    },
    {
      title: "Quota",
      dataIndex: "used_percent",
      width: 120,
      sorter: (a, b) => a.used_percent - b.used_percent,
      render: (value: number) => (value >= 0 ? `${value.toFixed(1)}%` : "-")
    },
    {
      title: "Requests",
      dataIndex: "attempt_requests",
      width: 140,
      sorter: (a, b) => a.attempt_requests - b.attempt_requests,
      render: (_, row) => `${formatNumber(row.successful_requests)}/${formatNumber(row.attempt_requests)}`
    },
    {
      title: "Errors",
      dataIndex: "attempt_errors",
      width: 110,
      sorter: (a, b) => a.attempt_errors - b.attempt_errors,
      render: (value: number) => formatNumber(value)
    },
    {
      title: "Tokens",
      dataIndex: ["usage", "total_tokens"],
      width: 130,
      sorter: (a, b) => a.usage.total_tokens - b.usage.total_tokens,
      render: (_, row) => formatNumber(row.usage.total_tokens)
    },
    {
      title: "Last Used",
      dataIndex: "last_used_at",
      width: 180,
      sorter: (a, b) => dateValue(a.last_used_at) - dateValue(b.last_used_at),
      render: (value?: string) => value ?? "-"
    }
  ];

  return (
    <Table
      columns={compact ? columns.slice(0, 5) : columns}
      dataSource={accounts}
      loading={loading && accounts.length === 0}
      locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No accounts" /> }}
      pagination={compact ? { pageSize: 8, showSizeChanger: false } : { pageSize: 20, showSizeChanger: true }}
      rowKey="file_path"
      scroll={{ x: compact ? 760 : 1060 }}
      size="middle"
    />
  );
}

function RequestLogsPage({
  loading,
  persistence,
  requestError,
  rows
}: {
  loading: boolean;
  persistence: PersistenceStatus | null;
  requestError: string | null;
  rows: RequestLog[];
}) {
  const columns: ColumnsType<RequestLog> = [
    {
      title: "Time",
      dataIndex: "ts_ms",
      width: 170,
      defaultSortOrder: "descend",
      sorter: (a, b) => a.ts_ms - b.ts_ms,
      render: (value: number) => formatDateTime(value)
    },
    {
      title: "Endpoint",
      dataIndex: "endpoint",
      width: 190,
      sorter: (a, b) => a.endpoint.localeCompare(b.endpoint)
    },
    {
      title: "Status",
      dataIndex: "status",
      width: 110,
      sorter: (a, b) => a.status - b.status,
      render: (value: number) => <StatusCodeTag value={value} />
    },
    {
      title: "Model",
      dataIndex: "model",
      width: 180,
      sorter: (a, b) => a.model.localeCompare(b.model)
    },
    {
      title: "Stream",
      dataIndex: "stream",
      width: 100,
      filters: [
        { text: "true", value: true },
        { text: "false", value: false }
      ],
      onFilter: (value, row) => row.stream === value,
      sorter: (a, b) => Number(a.stream) - Number(b.stream),
      render: (value: boolean) => (value ? "true" : "false")
    },
    {
      title: "Attempts",
      dataIndex: "attempts",
      width: 110,
      sorter: (a, b) => a.attempts - b.attempts
    },
    {
      title: "Duration",
      dataIndex: "duration_ms",
      width: 120,
      sorter: (a, b) => a.duration_ms - b.duration_ms,
      render: (value: number) => `${value} ms`
    },
    {
      title: "Account",
      dataIndex: "account_file_path",
      width: 220,
      sorter: (a, b) => (a.account_file_path ?? "").localeCompare(b.account_file_path ?? ""),
      render: (value?: string) => basename(value)
    },
    {
      title: "Error",
      dataIndex: "error_message",
      width: 260,
      sorter: (a, b) => (a.error_message ?? "").localeCompare(b.error_message ?? ""),
      render: (value?: string) => value ?? "-"
    }
  ];

  return (
    <Space className="page-stack" direction="vertical" size={16}>
      <LogNotice error={requestError} persistence={persistence} />
      <Card title="Request Logs">
        <Table
          columns={columns}
          dataSource={rows}
          loading={loading && rows.length === 0}
          locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No request logs" /> }}
          pagination={{ pageSize: 20, showSizeChanger: true }}
          rowKey="id"
          scroll={{ x: 1460 }}
          size="middle"
        />
      </Card>
    </Space>
  );
}

function UsageLogsPage({
  loading,
  persistence,
  rows,
  usageError
}: {
  loading: boolean;
  persistence: PersistenceStatus | null;
  rows: UsageLog[];
  usageError: string | null;
}) {
  const columns: ColumnsType<UsageLog> = [
    {
      title: "Time",
      dataIndex: "ts_ms",
      width: 170,
      defaultSortOrder: "descend",
      sorter: (a, b) => a.ts_ms - b.ts_ms,
      render: (value: number) => formatDateTime(value)
    },
    {
      title: "Endpoint",
      dataIndex: "endpoint",
      width: 190,
      sorter: (a, b) => a.endpoint.localeCompare(b.endpoint)
    },
    {
      title: "Model",
      dataIndex: "model",
      width: 180,
      sorter: (a, b) => a.model.localeCompare(b.model)
    },
    {
      title: "Input",
      dataIndex: "input_tokens",
      width: 120,
      sorter: (a, b) => a.input_tokens - b.input_tokens,
      render: (value: number) => formatNumber(value)
    },
    {
      title: "Output",
      dataIndex: "output_tokens",
      width: 120,
      sorter: (a, b) => a.output_tokens - b.output_tokens,
      render: (value: number) => formatNumber(value)
    },
    {
      title: "Cached",
      dataIndex: "cached_tokens",
      width: 120,
      sorter: (a, b) => a.cached_tokens - b.cached_tokens,
      render: (value: number) => formatNumber(value)
    },
    {
      title: "Reasoning",
      dataIndex: "reasoning_tokens",
      width: 130,
      sorter: (a, b) => a.reasoning_tokens - b.reasoning_tokens,
      render: (value: number) => formatNumber(value)
    },
    {
      title: "Total",
      dataIndex: "total_tokens",
      width: 120,
      sorter: (a, b) => a.total_tokens - b.total_tokens,
      render: (value: number) => formatNumber(value)
    },
    {
      title: "Account",
      dataIndex: "account_file_path",
      width: 220,
      sorter: (a, b) => a.account_file_path.localeCompare(b.account_file_path),
      render: (value: string) => basename(value)
    }
  ];

  return (
    <Space className="page-stack" direction="vertical" size={16}>
      <LogNotice error={usageError} persistence={persistence} />
      <Card title="Usage Logs">
        <Table
          columns={columns}
          dataSource={rows}
          loading={loading && rows.length === 0}
          locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No usage logs" /> }}
          pagination={{ pageSize: 20, showSizeChanger: true }}
          rowKey="id"
          scroll={{ x: 1370 }}
          size="middle"
        />
      </Card>
    </Space>
  );
}

function RuntimePage({
  limits,
  loading,
  persistence,
  runtimeError
}: {
  limits: RateLimits | null;
  loading: boolean;
  persistence: PersistenceStatus | null;
  runtimeError: string | null;
}) {
  const limitRows = [
    ["Key RPM", limits?.key_rpm],
    ["Key Concurrency", limits?.key_concurrency],
    ["Account RPM", limits?.account_rpm],
    ["Account Concurrency", limits?.account_concurrency],
    ["Image Concurrency", limits?.image_concurrency]
  ];

  const persistRows = [
    ["Enabled", persistence?.enabled ? "yes" : "no"],
    ["SQLite Writer", persistence?.enabled ? (persistence.writer_running ? "running" : "stopped") : "-"],
    ["Dropped Logs", formatNumber(persistence?.dropped_events ?? 0)],
    ["Write Errors", formatNumber(persistence?.write_errors ?? 0)]
  ];

  return (
    <Space className="page-stack" direction="vertical" size={16}>
      {runtimeError && <Alert message="Runtime status unavailable" description={runtimeError} type="error" showIcon />}
      <div className="runtime-grid">
        <Card loading={loading && !limits} title="Rate Limits">
          <KeyValueList rows={limitRows.map(([label, value]) => [label, limitText(value as number | undefined)])} />
        </Card>
        <Card loading={loading && !persistence} title="Persistence">
          <KeyValueList rows={persistRows} />
        </Card>
      </div>
    </Space>
  );
}

function SettingsPage({
  adminStatus,
  loading,
  onChangePassword
}: {
  adminStatus: AdminStatus | null;
  loading: boolean;
  onChangePassword: (values: { current_password: string; new_password: string }) => Promise<void>;
}) {
  const [form] = Form.useForm();
  return (
    <Space className="page-stack" direction="vertical" size={16}>
      <Card title="Admin Account">
        <KeyValueList
          rows={[
            ["Username", adminStatus?.username ?? "-"],
            ["Password", adminStatus?.initialized ? "configured" : "not configured"]
          ]}
        />
      </Card>
      <Card title="Change Password">
        <Form
          className="settings-form"
          form={form}
          layout="vertical"
          onFinish={async (values) => {
            await onChangePassword(values as { current_password: string; new_password: string });
            form.resetFields();
          }}
        >
          <Form.Item
            label="Current password"
            name="current_password"
            rules={[{ required: true, message: "Current password is required" }]}
          >
            <Input.Password prefix={<LockOutlined />} />
          </Form.Item>
          <Form.Item
            label="New password"
            name="new_password"
            rules={[
              { required: true, message: "New password is required" },
              { min: 8, message: "Password must be at least 8 characters" }
            ]}
          >
            <Input.Password prefix={<LockOutlined />} />
          </Form.Item>
          <Button htmlType="submit" loading={loading} type="primary">
            Update Password
          </Button>
        </Form>
      </Card>
    </Space>
  );
}

function LogNotice({ error, persistence }: { error: string | null; persistence: PersistenceStatus | null }) {
  if (error) {
    return <Alert message="Logs unavailable" description={error} type="error" showIcon />;
  }
  if (persistence && !persistence.enabled) {
    return (
      <Alert
        message="Persistence is disabled"
        description="Enable persistence.enabled in config.yaml before request and usage logs can be stored."
        type="warning"
        showIcon
      />
    );
  }
  return null;
}

function KeyValueList({ rows }: { rows: Array<[React.ReactNode, React.ReactNode]> }) {
  return (
    <div className="kv-list">
      {rows.map(([label, value]) => (
        <div className="kv-row" key={String(label)}>
          <Typography.Text type="secondary">{label}</Typography.Text>
          <Typography.Text strong>{value}</Typography.Text>
        </div>
      ))}
    </div>
  );
}

function StatusTag({ value }: { value: string }) {
  const color = value === "active" ? "green" : value === "cooldown" ? "gold" : "default";
  return <Tag color={color}>{value}</Tag>;
}

function StatusCodeTag({ value }: { value: number }) {
  const color = value >= 500 ? "red" : value >= 400 ? "orange" : value >= 300 ? "blue" : "green";
  return <Tag color={color}>{value}</Tag>;
}

function normalizeError(err: unknown) {
  const errorMessage = err instanceof Error ? err.message : String(err);
  if (errorMessage.includes("401")) {
    return "Invalid username, password, or admin session.";
  }
  return errorMessage;
}

function isAuthError(errorMessage: string) {
  return errorMessage.includes("Invalid username") || errorMessage.includes("401") || errorMessage.includes("admin session");
}

function displayAccount(row: AccountRow) {
  return row.email || basename(row.file_path);
}

function basename(path?: string) {
  if (!path) return "-";
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

function dateValue(value?: string) {
  if (!value) return 0;
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? 0 : parsed;
}

function formatDateTime(ms: number) {
  if (!ms) return "-";
  return new Date(ms).toLocaleString();
}

function formatNumber(value: number) {
  return value.toLocaleString();
}

function limitText(value?: number) {
  if (typeof value !== "number" || value === 0) {
    return "off";
  }
  return formatNumber(value);
}

const pageTitles: Record<PageKey, string> = {
  overview: "Overview",
  accounts: "Accounts",
  requests: "Request Logs",
  usage: "Usage Logs",
  runtime: "Runtime",
  settings: "Settings"
};

const pageSubtitles: Record<PageKey, string> = {
  overview: "Traffic summary and account snapshot",
  accounts: "Account health, quota, request counts, and token usage",
  requests: "Persisted request activity with sortable columns",
  usage: "Persisted token usage with sortable columns",
  runtime: "Rate limits and SQLite persistence status",
  settings: "Admin account and password"
};

createRoot(document.getElementById("root")!).render(<App />);
