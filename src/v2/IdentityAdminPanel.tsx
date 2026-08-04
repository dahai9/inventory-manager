import { type FormEvent, useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  ChevronLeft,
  ChevronRight,
  KeyRound,
  LoaderCircle,
  RefreshCw,
  Search,
  ShieldCheck,
  UserPlus,
  UserRoundX,
  Users,
} from "lucide-react";

type Notice = { type: "success" | "error"; text: string };
const PAGE_SIZE = 50;

interface TenantRole {
  role_id: string;
  code: string;
  name: string;
  description: string | null;
  system_role: boolean;
  permission_codes: string[];
}

interface TenantUser {
  user_id: string;
  login: string;
  display_name: string;
  email: string | null;
  account_status: string;
  membership_id: string;
  membership_status: string;
  consumes_license_seat: boolean;
  role_codes: string[];
  created_at: string;
  updated_at: string;
}

interface ListTenantUsersResponse {
  users: TenantUser[];
  next_after_user_id: string | null;
}

interface MembershipPermissions {
  user_id: string;
  membership_id: string;
  account_status: string;
  membership_status: string;
  role_codes: string[];
  permission_codes: string[];
}

interface IdentityAdminPanelProps {
  currentUserId: string | null;
}

function displayError(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  if (typeof error === "object" && error !== null) {
    const message = (error as { message?: unknown }).message;
    if (typeof message === "string") return message;
  }
  return String(error);
}

function createId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function roleIdsForUser(user: TenantUser, roles: TenantRole[]): Set<string> {
  const codes = new Set(user.role_codes);
  return new Set(roles.filter((role) => codes.has(role.code)).map((role) => role.role_id));
}

export default function IdentityAdminPanel({ currentUserId }: IdentityAdminPanelProps) {
  const [users, setUsers] = useState<TenantUser[]>([]);
  const [roles, setRoles] = useState<TenantRole[]>([]);
  const [search, setSearch] = useState("");
  const [includeDisabled, setIncludeDisabled] = useState(false);
  const [appliedSearch, setAppliedSearch] = useState("");
  const [appliedIncludeDisabled, setAppliedIncludeDisabled] = useState(false);
  const [afterUserId, setAfterUserId] = useState<string | null>(null);
  const [cursorHistory, setCursorHistory] = useState<Array<string | null>>([]);
  const [nextAfterUserId, setNextAfterUserId] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);

  const [login, setLogin] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [createRoleIds, setCreateRoleIds] = useState<Set<string>>(() => new Set());

  const [selectedUserId, setSelectedUserId] = useState<string | null>(null);
  const [editRoleIds, setEditRoleIds] = useState<Set<string>>(() => new Set());
  const [effectivePermissions, setEffectivePermissions] = useState<MembershipPermissions | null>(null);

  const selectedUser = useMemo(
    () => users.find((user) => user.user_id === selectedUserId) ?? null,
    [selectedUserId, users],
  );

  const refresh = useCallback(async (): Promise<boolean> => {
    setLoading(true);
    setNotice(null);
    try {
      const [userResponse, roleResponse] = await Promise.all([
        invoke<ListTenantUsersResponse>("v2_network_list_tenant_users", {
          input: {
            search: appliedSearch || null,
            include_disabled: appliedIncludeDisabled,
            after_user_id: afterUserId,
            limit: PAGE_SIZE,
          },
        }),
        invoke<TenantRole[]>("v2_network_list_tenant_roles"),
      ]);
      setUsers(userResponse.users);
      setNextAfterUserId(userResponse.next_after_user_id);
      setRoles(roleResponse);
      setSelectedUserId((current) => userResponse.users.some((user) => user.user_id === current) ? current : null);
      return true;
    } catch (error) {
      setNotice({ type: "error", text: `读取用户与角色失败：${displayError(error)}` });
      return false;
    } finally {
      setLoading(false);
    }
  }, [afterUserId, appliedIncludeDisabled, appliedSearch]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  function applyFilters(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const nextSearch = search.trim();
    const filtersChanged = nextSearch !== appliedSearch || includeDisabled !== appliedIncludeDisabled;
    setCursorHistory([]);
    setAfterUserId(null);
    setAppliedSearch(nextSearch);
    setAppliedIncludeDisabled(includeDisabled);
    if (!filtersChanged && afterUserId === null) void refresh();
  }

  function showPreviousPage() {
    if (loading || cursorHistory.length === 0) return;
    const previous = cursorHistory[cursorHistory.length - 1] ?? null;
    setCursorHistory((current) => current.slice(0, -1));
    setAfterUserId(previous);
  }

  function showNextPage() {
    if (loading || nextAfterUserId === null) return;
    setCursorHistory((current) => [...current, afterUserId]);
    setAfterUserId(nextAfterUserId);
  }

  async function selectUser(user: TenantUser) {
    setSelectedUserId(user.user_id);
    setEditRoleIds(roleIdsForUser(user, roles));
    setEffectivePermissions(null);
    setNotice(null);
    try {
      setEffectivePermissions(await invoke<MembershipPermissions>("v2_network_membership_permissions", {
        input: { membership_id: user.membership_id },
      }));
    } catch (error) {
      setNotice({ type: "error", text: `读取有效权限失败：${displayError(error)}` });
    }
  }

  function toggleRole(setter: React.Dispatch<React.SetStateAction<Set<string>>>, roleId: string) {
    setter((current) => {
      const next = new Set(current);
      if (next.has(roleId)) next.delete(roleId);
      else next.add(roleId);
      return next;
    });
  }

  async function createUser(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setNotice(null);
    if (!login.trim() || !displayName.trim() || password.length < 12 || createRoleIds.size === 0) {
      setNotice({ type: "error", text: "请填写账号、姓名、至少 12 字节的初始密码，并选择至少一个角色。" });
      return;
    }
    setLoading(true);
    try {
      await invoke("v2_network_create_tenant_user", {
        input: {
          request_id: createId(),
          login: login.trim(),
          display_name: displayName.trim(),
          email: email.trim() || null,
          password,
          role_ids: Array.from(createRoleIds),
        },
      });
      setLogin("");
      setDisplayName("");
      setEmail("");
      setPassword("");
      setCreateRoleIds(new Set());
      if (await refresh()) {
        setNotice({ type: "success", text: "用户已创建并占用一个有效授权席位。" });
      }
    } catch (error) {
      setNotice({ type: "error", text: `创建用户失败：${displayError(error)}` });
    } finally {
      setLoading(false);
    }
  }

  async function saveRoles() {
    if (!selectedUser) return;
    setLoading(true);
    setNotice(null);
    try {
      const response = await invoke<MembershipPermissions>("v2_network_replace_membership_roles", {
        input: {
          request_id: createId(),
          membership_id: selectedUser.membership_id,
          role_ids: Array.from(editRoleIds),
        },
      });
      setEffectivePermissions(response);
      if (await refresh()) {
        setNotice({ type: "success", text: "成员角色与有效权限已更新。" });
      }
    } catch (error) {
      setNotice({ type: "error", text: `更新角色失败：${displayError(error)}` });
    } finally {
      setLoading(false);
    }
  }

  async function disableUser(user: TenantUser) {
    if (user.user_id === currentUserId) return;
    const approved = await confirm(
      `禁用 ${user.display_name} 后，其现有访问会话和刷新令牌会立即失效。`,
      { title: "确认禁用用户", kind: "warning" },
    );
    if (!approved) return;
    setLoading(true);
    setNotice(null);
    try {
      const response = await invoke<{ revoked_session_count: number }>("v2_network_disable_tenant_user", {
        input: { request_id: createId(), user_id: user.user_id },
      });
      setSelectedUserId(null);
      setEffectivePermissions(null);
      if (await refresh()) {
        setNotice({ type: "success", text: `用户已禁用，撤销 ${response.revoked_session_count} 个访问会话。` });
      }
    } catch (error) {
      setNotice({ type: "error", text: `禁用用户失败：${displayError(error)}` });
    } finally {
      setLoading(false);
    }
  }

  return (
    <section className="v2-page" aria-labelledby="v2-users-title">
      <div className="v2-page-heading">
        <div><span className="v2-eyebrow">租户权限</span><h2 id="v2-users-title">用户与角色</h2><p>账号、成员关系、授权席位和有效权限由服务端统一校验。</p></div>
        <button className="v2-button" type="button" onClick={() => void refresh()} disabled={loading}><RefreshCw size={16} className={loading ? "v2-spin" : ""} /> 刷新</button>
      </div>
      {notice && <div className={`v2-notice ${notice.type}`}>{notice.text}</div>}

      <div className="v2-identity-layout">
        <section className="v2-panel v2-identity-directory">
          <form className="v2-identity-filter" onSubmit={applyFilters}>
            <label className="v2-search"><Search size={17} /><input value={search} onChange={(event) => setSearch(event.target.value)} placeholder="账号、姓名或邮箱" /></label>
            <label className="v2-check-label"><input type="checkbox" checked={includeDisabled} onChange={(event) => setIncludeDisabled(event.target.checked)} />显示已禁用</label>
            <button className="v2-button primary" type="submit">查询</button>
          </form>
          <div className="v2-table-wrap">
            <table className="v2-user-table">
              <thead><tr><th>用户</th><th>角色</th><th>状态</th><th aria-label="操作" /></tr></thead>
              <tbody>
                {!loading && users.length === 0 && <tr><td className="v2-table-empty" colSpan={4}>没有匹配的租户用户</td></tr>}
                {users.map((user) => <tr key={user.user_id} className={selectedUserId === user.user_id ? "selected" : ""}>
                  <td><strong>{user.display_name}</strong><small>{user.login}{user.email ? ` · ${user.email}` : ""}</small></td>
                  <td>{user.role_codes.length > 0 ? user.role_codes.join("、") : "未分配"}</td>
                  <td><span className={`v2-badge identity-${user.account_status}`}>{user.account_status === "active" ? "有效" : "已禁用"}</span></td>
                  <td><div className="v2-row-actions"><button className="v2-icon-button" type="button" onClick={() => void selectUser(user)} aria-label={`编辑 ${user.display_name} 的角色`} title="编辑角色"><ShieldCheck size={16} /></button><button className="v2-icon-button danger" type="button" disabled={user.account_status !== "active" || user.user_id === currentUserId} onClick={() => void disableUser(user)} aria-label={`禁用 ${user.display_name}`} title={user.user_id === currentUserId ? "不能在当前会话禁用自己" : "禁用用户"}><UserRoundX size={16} /></button></div></td>
                </tr>)}
              </tbody>
            </table>
          </div>
          <div className="v2-pagination">
            <button className="v2-icon-button" type="button" onClick={showPreviousPage} disabled={loading || cursorHistory.length === 0} aria-label="上一页" title="上一页"><ChevronLeft size={17} /></button>
            <span>第 {cursorHistory.length + 1} 页</span>
            <button className="v2-icon-button" type="button" onClick={showNextPage} disabled={loading || nextAfterUserId === null} aria-label="下一页" title="下一页"><ChevronRight size={17} /></button>
          </div>
        </section>

        <aside className="v2-identity-side">
          <form className="v2-panel v2-identity-create" onSubmit={createUser}>
            <div className="v2-settings-heading"><div><h3>新建用户</h3><small>创建账号、成员关系和初始角色</small></div><UserPlus size={20} /></div>
            <label><span>登录账号 *</span><input value={login} onChange={(event) => setLogin(event.target.value)} autoComplete="off" /></label>
            <label><span>显示姓名 *</span><input value={displayName} onChange={(event) => setDisplayName(event.target.value)} autoComplete="off" /></label>
            <label><span>邮箱</span><input type="email" value={email} onChange={(event) => setEmail(event.target.value)} autoComplete="off" /></label>
            <label><span>初始密码 *</span><input type="password" minLength={12} value={password} onChange={(event) => setPassword(event.target.value)} autoComplete="new-password" /></label>
            <RoleChecklist roles={roles} selected={createRoleIds} onToggle={(roleId) => toggleRole(setCreateRoleIds, roleId)} />
            <button className="v2-button primary wide" type="submit" disabled={loading}><UserPlus size={16} /> 创建用户</button>
          </form>

          {selectedUser && <section className="v2-panel v2-identity-editor">
            <div className="v2-settings-heading"><div><h3>{selectedUser.display_name}</h3><small>{selectedUser.login} · {selectedUser.membership_id}</small></div><Users size={20} /></div>
            <RoleChecklist roles={roles} selected={editRoleIds} onToggle={(roleId) => toggleRole(setEditRoleIds, roleId)} />
            <button className="v2-button primary wide" type="button" onClick={() => void saveRoles()} disabled={loading}><ShieldCheck size={16} /> 保存角色</button>
            <div className="v2-effective-permissions"><strong><KeyRound size={15} /> 有效权限</strong>{effectivePermissions ? (effectivePermissions.permission_codes.length > 0 ? <div>{effectivePermissions.permission_codes.map((code) => <span key={code}>{code}</span>)}</div> : <small>当前没有有效权限</small>) : <small>{loading ? <LoaderCircle className="v2-spin" size={15} /> : "正在读取"}</small>}</div>
          </section>}
        </aside>
      </div>
    </section>
  );
}

interface RoleChecklistProps {
  roles: TenantRole[];
  selected: Set<string>;
  onToggle: (roleId: string) => void;
}

function RoleChecklist({ roles, selected, onToggle }: RoleChecklistProps) {
  return (
    <fieldset className="v2-role-checklist">
      <legend>角色</legend>
      {roles.map((role) => <label key={role.role_id}><input type="checkbox" checked={selected.has(role.role_id)} onChange={() => onToggle(role.role_id)} /><span><strong>{role.name}</strong><small>{role.code} · {role.permission_codes.length} 项权限</small></span></label>)}
    </fieldset>
  );
}
