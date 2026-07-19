import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser } from "./session";
import { Id, Doc } from "./_generated/dataModel";

/** Permission bitfield — keep in sync with the Rust client. */
export const Perm = {
  VIEW_CHANNELS: 1 << 0,
  SEND_MESSAGES: 1 << 1,
  MANAGE_CHANNELS: 1 << 2,
  KICK_MEMBERS: 1 << 3,
  MANAGE_ROLES: 1 << 4,
  MANAGE_SERVER: 1 << 5,
  CONNECT_VOICE: 1 << 6,
  SPEAK: 1 << 7,
  /** Post in announcement / staff-read-only channels. */
  ANNOUNCE: 1 << 8,
} as const;

export const DEFAULT_EVERYONE_PERMS =
  Perm.VIEW_CHANNELS |
  Perm.SEND_MESSAGES |
  Perm.CONNECT_VOICE |
  Perm.SPEAK;

/** Default "Staff" role on new servers — manage + announce, not kick. */
export const DEFAULT_STAFF_PERMS =
  DEFAULT_EVERYONE_PERMS |
  Perm.MANAGE_CHANNELS |
  Perm.MANAGE_ROLES |
  Perm.ANNOUNCE;

export const ALL_PERMS =
  Perm.VIEW_CHANNELS |
  Perm.SEND_MESSAGES |
  Perm.MANAGE_CHANNELS |
  Perm.KICK_MEMBERS |
  Perm.MANAGE_ROLES |
  Perm.MANAGE_SERVER |
  Perm.CONNECT_VOICE |
  Perm.SPEAK |
  Perm.ANNOUNCE;

const ROLE_COLORS = [
  "#33FF66",
  "#88FFAA",
  "#00CC55",
  "#CCFF33",
  "#66DD88",
  "#FAA61A",
  "#EB459E",
  "#ED4245",
];

export async function requireServerMember(
  ctx: QueryCtx | MutationCtx,
  serverId: Id<"servers">,
  userId: Id<"users">,
) {
  const membership = await ctx.db
    .query("serverMembers")
    .withIndex("by_server_and_user", (q) =>
      q.eq("serverId", serverId).eq("userId", userId),
    )
    .unique();
  if (!membership) {
    throw new Error("You're not a member of this server");
  }
  return membership;
}

/** Roles explicitly assigned to a member — multi-role, falling back to the
 * old single `roleId` field for members assigned before this existed. */
export function assignedRoleIds(membership: Doc<"serverMembers">): Id<"serverRoles">[] {
  if (membership.roleIds) return membership.roleIds;
  return membership.roleId ? [membership.roleId] : [];
}

/** Every member implicitly holds the server's @everyone (position 0) role,
 * plus the union of whatever else is explicitly assigned — same model as
 * Discord, where permissions are additive across all held roles. */
export async function memberPermissions(
  ctx: QueryCtx | MutationCtx,
  server: Doc<"servers">,
  membership: Doc<"serverMembers">,
): Promise<number> {
  if (server.ownerId === membership.userId) {
    return ALL_PERMS;
  }
  const allRoles = await ctx.db
    .query("serverRoles")
    .withIndex("by_server", (q) => q.eq("serverId", server._id))
    .take(50);
  const everyoneRole = allRoles.find((r) => r.position === 0);
  let perms = everyoneRole?.permissions ?? DEFAULT_EVERYONE_PERMS;

  const assigned = new Set(assignedRoleIds(membership).map(String));
  for (const role of allRoles) {
    if (assigned.has(String(role._id))) {
      perms |= role.permissions;
    }
  }
  return perms;
}

/** Highest position among the member's assigned roles (0 = only the
 * implicit @everyone role) — the member's rung on the role hierarchy. */
export async function highestRolePosition(
  ctx: QueryCtx | MutationCtx,
  membership: Doc<"serverMembers">,
): Promise<number> {
  const allRoles = await ctx.db
    .query("serverRoles")
    .withIndex("by_server", (q) => q.eq("serverId", membership.serverId))
    .take(50);
  const assigned = new Set(assignedRoleIds(membership).map(String));
  let highest = 0;
  for (const role of allRoles) {
    if (assigned.has(String(role._id))) {
      highest = Math.max(highest, role.position);
    }
  }
  return highest;
}

export async function requirePerm(
  ctx: QueryCtx | MutationCtx,
  serverId: Id<"servers">,
  userId: Id<"users">,
  perm: number,
) {
  const server = await ctx.db.get("servers", serverId);
  if (!server) throw new Error("Server not found");
  const membership = await requireServerMember(ctx, serverId, userId);
  const perms = await memberPermissions(ctx, server, membership);
  if ((perms & perm) !== perm) {
    throw new Error("Missing permission");
  }
  return { server, membership, perms };
}

/**
 * Discord-style channel permission resolution:
 * 1) base = union of @everyone + assigned roles (owner = ALL)
 * 2) apply role overwrites: perms = (perms & ~deny) | allow
 * 3) apply member overwrite last
 */
export async function channelPermissions(
  ctx: QueryCtx | MutationCtx,
  conversationId: Id<"conversations">,
  userId: Id<"users">,
): Promise<number> {
  const conversation = await ctx.db.get("conversations", conversationId);
  if (!conversation || conversation.kind !== "channel" || !conversation.serverId) {
    // DMs / groups: full chat rights for members.
    return ALL_PERMS;
  }
  const server = await ctx.db.get("servers", conversation.serverId);
  if (!server) return 0;
  const membership = await requireServerMember(ctx, conversation.serverId, userId);
  let perms = await memberPermissions(ctx, server, membership);

  const overwrites = await ctx.db
    .query("channelOverwrites")
    .withIndex("by_conversation", (q) => q.eq("conversationId", conversationId))
    .take(100);

  const assigned = new Set(assignedRoleIds(membership).map(String));
  // Also treat @everyone (position 0) as a role overwrite target.
  const allRoles = await ctx.db
    .query("serverRoles")
    .withIndex("by_server", (q) => q.eq("serverId", conversation.serverId!))
    .take(50);
  const everyone = allRoles.find((r) => r.position === 0);

  // Role overwrites first (everyone, then assigned roles by position).
  const roleOw = overwrites.filter((o) => o.targetType === "role");
  const orderedRoleIds: string[] = [];
  if (everyone) orderedRoleIds.push(String(everyone._id));
  for (const r of allRoles
    .filter((r) => r.position !== 0)
    .sort((a, b) => a.position - b.position)) {
    if (assigned.has(String(r._id))) orderedRoleIds.push(String(r._id));
  }
  for (const roleId of orderedRoleIds) {
    const ow = roleOw.find((o) => o.targetId === roleId);
    if (!ow) continue;
    perms = (perms & ~ow.deny) | ow.allow;
  }

  const memberOw = overwrites.find(
    (o) => o.targetType === "member" && o.targetId === String(userId),
  );
  if (memberOw) {
    perms = (perms & ~memberOw.deny) | memberOw.allow;
  }

  // Announcement channels: SEND_MESSAGES only via ANNOUNCE (or full manage).
  if (conversation.isAnnouncement) {
    const mayAnnounce =
      (perms & Perm.ANNOUNCE) === Perm.ANNOUNCE ||
      (perms & Perm.MANAGE_SERVER) === Perm.MANAGE_SERVER ||
      server.ownerId === userId;
    if (!mayAnnounce) {
      perms &= ~Perm.SEND_MESSAGES;
    } else {
      perms |= Perm.SEND_MESSAGES;
    }
  }

  return perms;
}

export async function requireChannelPerm(
  ctx: QueryCtx | MutationCtx,
  conversationId: Id<"conversations">,
  userId: Id<"users">,
  perm: number,
) {
  const perms = await channelPermissions(ctx, conversationId, userId);
  if ((perms & perm) !== perm) {
    throw new Error("Missing permission");
  }
  return perms;
}

/** Ensure a brand-new server has an @everyone-style default role. */
export async function ensureDefaultRole(
  ctx: MutationCtx,
  serverId: Id<"servers">,
) {
  const existing = await ctx.db
    .query("serverRoles")
    .withIndex("by_server", (q) => q.eq("serverId", serverId))
    .take(1);
  if (existing.length > 0) return existing[0]._id;
  return await ctx.db.insert("serverRoles", {
    serverId,
    name: "everyone",
    color: "#33FF66",
    position: 0,
    permissions: DEFAULT_EVERYONE_PERMS,
  });
}

export const listRoles = query({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireServerMember(ctx, args.serverId, me._id);
    const roles = await ctx.db
      .query("serverRoles")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(50);
    return roles
      .map((r) => ({
        roleId: r._id,
        name: r.name,
        color: r.color,
        position: r.position,
        permissions: r.permissions,
      }))
      .sort((a, b) => b.position - a.position || a.name.localeCompare(b.name));
  },
});

export const createRole = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    name: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requirePerm(ctx, args.serverId, me._id, Perm.MANAGE_ROLES);
    const name = args.name.trim().slice(0, 32);
    if (name.length === 0) throw new Error("Enter a role name");

    const existing = await ctx.db
      .query("serverRoles")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(50);
    const position =
      existing.reduce((max, r) => Math.max(max, r.position), 0) + 1;
    const color = ROLE_COLORS[existing.length % ROLE_COLORS.length];

    return await ctx.db.insert("serverRoles", {
      serverId: args.serverId,
      name,
      color,
      position,
      permissions: DEFAULT_EVERYONE_PERMS,
    });
  },
});

export const updateRole = mutation({
  args: {
    sessionToken: v.string(),
    roleId: v.id("serverRoles"),
    name: v.optional(v.string()),
    color: v.optional(v.string()),
    permissions: v.optional(v.number()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const role = await ctx.db.get("serverRoles", args.roleId);
    if (!role) throw new Error("Role not found");
    const { server, membership, perms } = await requirePerm(
      ctx,
      role.serverId,
      me._id,
      Perm.MANAGE_ROLES,
    );

    // Discord-style hierarchy rules; the server owner bypasses them all.
    if (server.ownerId !== me._id) {
      if (role.position === 0) {
        // The implicit @everyone role is server-wide — editing it takes
        // Manage Server, not just Manage Roles.
        if ((perms & Perm.MANAGE_SERVER) !== Perm.MANAGE_SERVER) {
          throw new Error("Editing the everyone role requires Manage Server");
        }
      } else if (role.position >= (await highestRolePosition(ctx, membership))) {
        throw new Error("You can't edit a role at or above your highest role");
      }
      // Never grant permission bits you don't hold yourself.
      if (
        args.permissions !== undefined &&
        (args.permissions & ALL_PERMS & ~perms) !== 0
      ) {
        throw new Error("You can't grant permissions you don't have");
      }
    }

    const patch: {
      name?: string;
      color?: string;
      permissions?: number;
    } = {};
    if (args.name !== undefined) {
      const name = args.name.trim().slice(0, 32);
      if (name.length === 0) throw new Error("Enter a role name");
      patch.name = name;
    }
    if (args.color !== undefined) {
      if (!/^#[0-9A-Fa-f]{6}$/.test(args.color)) {
        throw new Error("Invalid color");
      }
      patch.color = args.color;
    }
    if (args.permissions !== undefined) {
      patch.permissions = args.permissions & ALL_PERMS;
    }
    await ctx.db.patch("serverRoles", role._id, patch);
    return null;
  },
});

export const deleteRole = mutation({
  args: {
    sessionToken: v.string(),
    roleId: v.id("serverRoles"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const role = await ctx.db.get("serverRoles", args.roleId);
    if (!role) throw new Error("Role not found");
    await requirePerm(ctx, role.serverId, me._id, Perm.MANAGE_ROLES);
    if (role.name === "everyone" && role.position === 0) {
      throw new Error("Can't delete the default role");
    }

    const members = await ctx.db
      .query("serverMembers")
      .withIndex("by_server", (q) => q.eq("serverId", role.serverId))
      .take(500);
    for (const m of members) {
      const current = assignedRoleIds(m);
      if (current.some((id) => id === role._id)) {
        await ctx.db.patch("serverMembers", m._id, {
          roleIds: current.filter((id) => id !== role._id),
          roleId: undefined,
        });
      }
    }
    await ctx.db.delete("serverRoles", role._id);
    return null;
  },
});

/** Grants the role if the member doesn't have it, revokes it if they do —
 * a member can hold any number of roles at once (plus the implicit
 * @everyone baseline), same as Discord. */
export const toggleRole = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    userId: v.id("users"),
    roleId: v.id("serverRoles"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const { server, membership: myMembership } = await requirePerm(
      ctx,
      args.serverId,
      me._id,
      Perm.MANAGE_ROLES,
    );
    if (args.userId === server.ownerId) {
      throw new Error("Owner always has full permissions");
    }
    const membership = await requireServerMember(
      ctx,
      args.serverId,
      args.userId,
    );
    const role = await ctx.db.get("serverRoles", args.roleId);
    if (!role || role.serverId !== args.serverId) {
      throw new Error("Role not found on this server");
    }
    // Discord-style hierarchy: you can only hand out roles below your own
    // highest role; the server owner bypasses this.
    if (
      server.ownerId !== me._id &&
      role.position >= (await highestRolePosition(ctx, myMembership))
    ) {
      throw new Error("You can't assign a role at or above your highest role");
    }
    const current = assignedRoleIds(membership);
    const has = current.some((id) => id === args.roleId);
    const next = has
      ? current.filter((id) => id !== args.roleId)
      : [...current, args.roleId];
    await ctx.db.patch("serverMembers", membership._id, { roleIds: next });
    return null;
  },
});

export const myPermissions = query({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server) throw new Error("Server not found");
    const membership = await requireServerMember(ctx, args.serverId, me._id);
    return {
      permissions: await memberPermissions(ctx, server, membership),
      isOwner: server.ownerId === me._id,
    };
  },
});
