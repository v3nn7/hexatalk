import { v } from "convex/values";
import {
  internalMutation,
  mutation,
  query,
  MutationCtx,
  QueryCtx,
} from "./_generated/server";
import { Doc, Id } from "./_generated/dataModel";
import {
  currentUser,
  isBlockedEitherWay,
  isProtectedTarget,
  isStaff,
  platformRank,
  platformRole,
} from "./session";

const NOTE_MAX_LEN = 200;
const NICKNAME_MAX_LEN = 40;
const PRIVATE_NOTE_MAX_LEN = 300;
const ONLINE_MS = 90_000;
/** After a decline, sender must wait before re-requesting. */
export const DECLINE_COOLDOWN_MS = 24 * 60 * 60 * 1000;
/** Declined rows older than this are purged by cleanup. */
const DECLINED_RETENTION_MS = 30 * 24 * 60 * 60 * 1000;
/** Max pending outgoing requests at once. */
const MAX_PENDING_OUTGOING = 25;
/** Max friend requests sent per rolling hour. */
const MAX_REQUESTS_PER_HOUR = 20;

type PresenceKind = "online" | "idle" | "dnd" | "offline" | "invisible";

function normalizeNote(raw: string | undefined): string | undefined {
  if (raw === undefined) return undefined;
  const note = raw.trim().slice(0, NOTE_MAX_LEN);
  return note.length > 0 ? note : undefined;
}

function privacyOf(user: Doc<"users">): "everyone" | "mutual_servers" | "nobody" {
  return user.friendRequestPrivacy ?? "everyone";
}

async function avatarUrlFor(
  ctx: QueryCtx | MutationCtx,
  user: Doc<"users">,
): Promise<string> {
  if (!user.avatarStorageId) return "";
  return (await ctx.storage.getUrl(user.avatarStorageId)) ?? "";
}

async function listPushTokensForUser(
  ctx: QueryCtx | MutationCtx,
  userId: Id<"users">,
) {
  return await ctx.db
    .query("pushTokens")
    .withIndex("by_user", (q) => q.eq("userId", userId))
    .take(20);
}

async function shareMutualServer(
  ctx: QueryCtx | MutationCtx,
  a: Id<"users">,
  b: Id<"users">,
): Promise<boolean> {
  const names = await mutualServerNames(ctx, a, b, 1);
  return names.length > 0;
}

async function mutualServerNames(
  ctx: QueryCtx | MutationCtx,
  a: Id<"users">,
  b: Id<"users">,
  limit = 5,
): Promise<string[]> {
  const memberships = await ctx.db
    .query("serverMembers")
    .withIndex("by_user", (q) => q.eq("userId", a))
    .take(200);
  const names: string[] = [];
  for (const m of memberships) {
    if (names.length >= limit) break;
    const other = await ctx.db
      .query("serverMembers")
      .withIndex("by_server_and_user", (q) =>
        q.eq("serverId", m.serverId).eq("userId", b),
      )
      .unique();
    if (!other) continue;
    const server = await ctx.db.get("servers", m.serverId);
    if (server) names.push(server.name);
  }
  return names;
}

async function assertCanReceiveFriendRequest(
  ctx: QueryCtx | MutationCtx,
  sender: Doc<"users">,
  target: Doc<"users">,
): Promise<void> {
  const privacy = privacyOf(target);
  if (privacy === "nobody") {
    throw new Error("This user is not accepting friend requests");
  }
  if (privacy === "mutual_servers") {
    const ok = await shareMutualServer(ctx, sender._id, target._id);
    if (!ok) {
      throw new Error(
        "This user only accepts friend requests from people who share a server",
      );
    }
  }
}

function cooldownMessage(respondedAt: number): string {
  const remaining = DECLINE_COOLDOWN_MS - (Date.now() - respondedAt);
  if (remaining <= 0) return "You can send a request again now";
  const hours = Math.max(1, Math.ceil(remaining / (60 * 60 * 1000)));
  return `You can send another request in about ${hours}h`;
}

function effectivePresence(
  user: Doc<"users">,
  lastSeenAt: number,
  viewerIsSelf: boolean,
): { lastSeenAt: number; presence: PresenceKind } {
  const preferred = (user.presenceStatus ?? "online") as PresenceKind | "online";
  if (!viewerIsSelf && (user.hideOnlineStatus === true || preferred === "invisible")) {
    return { lastSeenAt: 0, presence: "offline" };
  }
  const isOnline = lastSeenAt > 0 && Date.now() - lastSeenAt < ONLINE_MS;
  if (!isOnline) return { lastSeenAt, presence: "offline" };
  if (preferred === "idle" || preferred === "dnd") {
    return { lastSeenAt, presence: preferred };
  }
  return { lastSeenAt, presence: "online" };
}

async function friendshipIds(
  ctx: QueryCtx | MutationCtx,
  meId: Id<"users">,
): Promise<Set<string>> {
  const asSender = await ctx.db
    .query("friendRequests")
    .withIndex("by_from_and_status", (q) =>
      q.eq("fromUserId", meId).eq("status", "accepted"),
    )
    .take(200);
  const asReceiver = await ctx.db
    .query("friendRequests")
    .withIndex("by_to_and_status", (q) =>
      q.eq("toUserId", meId).eq("status", "accepted"),
    )
    .take(200);
  const ids = new Set<string>();
  for (const r of asSender) ids.add(r.toUserId);
  for (const r of asReceiver) ids.add(r.fromUserId);
  return ids;
}

async function getMeta(
  ctx: QueryCtx | MutationCtx,
  ownerId: Id<"users">,
  friendId: Id<"users">,
) {
  return await ctx.db
    .query("friendMeta")
    .withIndex("by_owner_and_friend", (q) =>
      q.eq("ownerId", ownerId).eq("friendId", friendId),
    )
    .unique();
}

async function assertAreFriends(
  ctx: QueryCtx | MutationCtx,
  a: Id<"users">,
  b: Id<"users">,
): Promise<void> {
  const forward = await ctx.db
    .query("friendRequests")
    .withIndex("by_from_and_to", (q) => q.eq("fromUserId", a).eq("toUserId", b))
    .unique();
  const backward = await ctx.db
    .query("friendRequests")
    .withIndex("by_from_and_to", (q) => q.eq("fromUserId", b).eq("toUserId", a))
    .unique();
  if (forward?.status !== "accepted" && backward?.status !== "accepted") {
    throw new Error("You're not friends with this user");
  }
}

async function assertRateLimit(
  ctx: MutationCtx,
  meId: Id<"users">,
): Promise<void> {
  const pendingOut = await ctx.db
    .query("friendRequests")
    .withIndex("by_from_and_status", (q) =>
      q.eq("fromUserId", meId).eq("status", "pending"),
    )
    .take(MAX_PENDING_OUTGOING + 1);
  if (pendingOut.length >= MAX_PENDING_OUTGOING) {
    throw new Error(
      `You already have ${MAX_PENDING_OUTGOING} pending requests — cancel some first`,
    );
  }

  const hourAgo = Date.now() - 60 * 60 * 1000;
  // Scan recent outgoing of any status (pending/declined/accepted) by time.
  const recentish = await ctx.db
    .query("friendRequests")
    .withIndex("by_from_and_status", (q) =>
      q.eq("fromUserId", meId).eq("status", "pending"),
    )
    .take(50);
  const declined = await ctx.db
    .query("friendRequests")
    .withIndex("by_from_and_status", (q) =>
      q.eq("fromUserId", meId).eq("status", "declined"),
    )
    .take(50);
  const accepted = await ctx.db
    .query("friendRequests")
    .withIndex("by_from_and_status", (q) =>
      q.eq("fromUserId", meId).eq("status", "accepted"),
    )
    .take(50);
  const recentCount = [...recentish, ...declined, ...accepted].filter(
    (r) => (r.sentAt ?? r._creationTime) >= hourAgo,
  ).length;
  if (recentCount >= MAX_REQUESTS_PER_HOUR) {
    throw new Error("You're sending friend requests too quickly — try again later");
  }
}

// ─── Mutations ───────────────────────────────────────────────────────────────

export const sendRequest = mutation({
  args: {
    sessionToken: v.string(),
    toUsername: v.string(),
    note: v.optional(v.string()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const targetUsername = args.toUsername.trim().toLowerCase();
    const note = normalizeNote(args.note);
    const now = Date.now();

    if (targetUsername === me.username) {
      throw new Error("You can't add yourself");
    }

    const target = await ctx.db
      .query("users")
      .withIndex("by_username", (q) => q.eq("username", targetUsername))
      .unique();
    if (!target) {
      throw new Error("No user found with that username");
    }
    if (target.isBot) {
      throw new Error("You can't friend a bot — invite it to a server instead");
    }
    if (target.banned) {
      throw new Error("You can't send a request to this user");
    }

    if (await isBlockedEitherWay(ctx, me._id, target._id)) {
      throw new Error("You can't send a request to this user");
    }

    await assertCanReceiveFriendRequest(ctx, me, target);

    const existingForward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", me._id).eq("toUserId", target._id),
      )
      .unique();
    const existingBackward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", target._id).eq("toUserId", me._id),
      )
      .unique();

    if (
      existingForward?.status === "accepted" ||
      existingBackward?.status === "accepted"
    ) {
      throw new Error("You're already friends");
    }

    // Mutual accept: they already invited you.
    if (existingBackward?.status === "pending") {
      await ctx.db.patch("friendRequests", existingBackward._id, {
        status: "accepted",
        respondedAt: now,
      });
      if (existingForward) {
        await ctx.db.delete("friendRequests", existingForward._id);
      }
      return null;
    }

    if (existingForward?.status === "pending") {
      throw new Error("A request is already pending");
    }

    await assertRateLimit(ctx, me._id);

    if (existingForward?.status === "declined") {
      const respondedAt =
        existingForward.respondedAt ?? existingForward._creationTime;
      if (now - respondedAt < DECLINE_COOLDOWN_MS) {
        throw new Error(cooldownMessage(respondedAt));
      }
      await ctx.db.patch("friendRequests", existingForward._id, {
        status: "pending",
        note,
        sentAt: now,
        respondedAt: undefined,
      });
      await listPushTokensForUser(ctx, target._id);
      return null;
    }

    if (existingForward) {
      await ctx.db.patch("friendRequests", existingForward._id, {
        status: "pending",
        note,
        sentAt: now,
        respondedAt: undefined,
      });
      await listPushTokensForUser(ctx, target._id);
      return null;
    }

    await ctx.db.insert("friendRequests", {
      fromUserId: me._id,
      toUserId: target._id,
      status: "pending",
      note,
      sentAt: now,
    });
    await listPushTokensForUser(ctx, target._id);
    return null;
  },
});

export const cancelRequest = mutation({
  args: {
    sessionToken: v.string(),
    requestId: v.id("friendRequests"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const request = await ctx.db.get("friendRequests", args.requestId);
    if (!request || request.fromUserId !== me._id) {
      throw new Error("Request not found");
    }
    if (request.status !== "pending") {
      throw new Error("Only pending requests can be cancelled");
    }
    await ctx.db.delete("friendRequests", request._id);
    return null;
  },
});

export const respondRequest = mutation({
  args: {
    sessionToken: v.string(),
    requestId: v.id("friendRequests"),
    accept: v.boolean(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const request = await ctx.db.get("friendRequests", args.requestId);
    if (!request || request.toUserId !== me._id) {
      throw new Error("Request not found");
    }
    if (request.status !== "pending") {
      throw new Error("This request is no longer pending");
    }

    const now = Date.now();
    if (args.accept) {
      await ctx.db.patch("friendRequests", request._id, {
        status: "accepted",
        respondedAt: now,
      });
    } else {
      await ctx.db.patch("friendRequests", request._id, {
        status: "declined",
        respondedAt: now,
      });
    }
    return null;
  },
});

/** Accept/decline all pending incoming requests in one shot. */
export const respondAllIncoming = mutation({
  args: {
    sessionToken: v.string(),
    accept: v.boolean(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const requests = await ctx.db
      .query("friendRequests")
      .withIndex("by_to_and_status", (q) =>
        q.eq("toUserId", me._id).eq("status", "pending"),
      )
      .take(100);
    const now = Date.now();
    let count = 0;
    for (const request of requests) {
      if (args.accept) {
        await ctx.db.patch("friendRequests", request._id, {
          status: "accepted",
          respondedAt: now,
        });
      } else {
        await ctx.db.patch("friendRequests", request._id, {
          status: "declined",
          respondedAt: now,
        });
      }
      count += 1;
    }
    return { count };
  },
});

export const removeFriend = mutation({
  args: { sessionToken: v.string(), friendUserId: v.id("users") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);

    const forward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", me._id).eq("toUserId", args.friendUserId),
      )
      .unique();
    const backward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", args.friendUserId).eq("toUserId", me._id),
      )
      .unique();

    if (forward) await ctx.db.delete("friendRequests", forward._id);
    if (backward) await ctx.db.delete("friendRequests", backward._id);

    const meta = await getMeta(ctx, me._id, args.friendUserId);
    if (meta) await ctx.db.delete("friendMeta", meta._id);

    return null;
  },
});

export const setFriendMeta = mutation({
  args: {
    sessionToken: v.string(),
    friendUserId: v.id("users"),
    nickname: v.optional(v.string()),
    favorite: v.optional(v.boolean()),
    privateNote: v.optional(v.string()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await assertAreFriends(ctx, me._id, args.friendUserId);

    const nickname =
      args.nickname === undefined
        ? undefined
        : args.nickname.trim().slice(0, NICKNAME_MAX_LEN);
    const privateNote =
      args.privateNote === undefined
        ? undefined
        : args.privateNote.trim().slice(0, PRIVATE_NOTE_MAX_LEN);

    const existing = await getMeta(ctx, me._id, args.friendUserId);
    if (existing) {
      const patch: {
        nickname?: string;
        favorite?: boolean;
        privateNote?: string;
      } = {};
      if (args.nickname !== undefined) {
        patch.nickname = nickname && nickname.length > 0 ? nickname : undefined;
      }
      if (args.favorite !== undefined) patch.favorite = args.favorite;
      if (args.privateNote !== undefined) {
        patch.privateNote =
          privateNote && privateNote.length > 0 ? privateNote : undefined;
      }
      // Convex optional clear: set undefined via replace fields carefully.
      await ctx.db.patch("friendMeta", existing._id, {
        ...(args.nickname !== undefined
          ? { nickname: nickname && nickname.length > 0 ? nickname : "" }
          : {}),
        ...(args.favorite !== undefined ? { favorite: args.favorite } : {}),
        ...(args.privateNote !== undefined
          ? {
              privateNote:
                privateNote && privateNote.length > 0 ? privateNote : "",
            }
          : {}),
      });
      return null;
    }

    await ctx.db.insert("friendMeta", {
      ownerId: me._id,
      friendId: args.friendUserId,
      nickname: nickname && nickname.length > 0 ? nickname : undefined,
      favorite: args.favorite === true,
      privateNote: privateNote && privateNote.length > 0 ? privateNote : undefined,
    });
    return null;
  },
});

export const toggleFavorite = mutation({
  args: {
    sessionToken: v.string(),
    friendUserId: v.id("users"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await assertAreFriends(ctx, me._id, args.friendUserId);
    const existing = await getMeta(ctx, me._id, args.friendUserId);
    if (existing) {
      const next = existing.favorite !== true;
      await ctx.db.patch("friendMeta", existing._id, { favorite: next });
      return { favorite: next };
    }
    await ctx.db.insert("friendMeta", {
      ownerId: me._id,
      friendId: args.friendUserId,
      favorite: true,
    });
    return { favorite: true };
  },
});

export const setPresenceStatus = mutation({
  args: {
    sessionToken: v.string(),
    status: v.union(
      v.literal("online"),
      v.literal("idle"),
      v.literal("dnd"),
      v.literal("invisible"),
    ),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await ctx.db.patch("users", me._id, { presenceStatus: args.status });
    return null;
  },
});

export const blockUser = mutation({
  args: { sessionToken: v.string(), userId: v.id("users") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (args.userId === me._id) {
      throw new Error("You can't block yourself");
    }
    const target = await ctx.db.get("users", args.userId);
    if (!target) throw new Error("User not found");
    if (isProtectedTarget(target)) {
      throw new Error("You can't block HexaTalk staff");
    }
    if (platformRank(target) >= platformRank(me) && platformRank(target) >= 50) {
      throw new Error("You can't block staff at your rank or higher");
    }

    const forward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", me._id).eq("toUserId", args.userId),
      )
      .unique();
    const backward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", args.userId).eq("toUserId", me._id),
      )
      .unique();
    if (forward) await ctx.db.delete("friendRequests", forward._id);
    if (backward) await ctx.db.delete("friendRequests", backward._id);

    const meta = await getMeta(ctx, me._id, args.userId);
    if (meta) await ctx.db.delete("friendMeta", meta._id);

    const existing = await ctx.db
      .query("blocks")
      .withIndex("by_blocker_and_blocked", (q) =>
        q.eq("blockerId", me._id).eq("blockedId", args.userId),
      )
      .unique();
    if (!existing) {
      await ctx.db.insert("blocks", { blockerId: me._id, blockedId: args.userId });
    }
    return null;
  },
});

export const unblockUser = mutation({
  args: { sessionToken: v.string(), userId: v.id("users") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const existing = await ctx.db
      .query("blocks")
      .withIndex("by_blocker_and_blocked", (q) =>
        q.eq("blockerId", me._id).eq("blockedId", args.userId),
      )
      .unique();
    if (existing) {
      await ctx.db.delete("blocks", existing._id);
    }
    return null;
  },
});

// ─── Queries ─────────────────────────────────────────────────────────────────

export const listBlocked = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const blocks = await ctx.db
      .query("blocks")
      .withIndex("by_blocker", (q) => q.eq("blockerId", me._id))
      .take(200);

    const result = [];
    for (const block of blocks) {
      const user = await ctx.db.get("users", block.blockedId);
      if (user) {
        result.push({
          userId: user._id,
          username: user.username,
          displayName: user.displayName,
          avatarColor: user.avatarColor ?? "",
          avatarImageUrl: await avatarUrlFor(ctx, user),
        });
      }
    }
    return result;
  },
});

export const listIncomingRequests = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const requests = await ctx.db
      .query("friendRequests")
      .withIndex("by_to_and_status", (q) =>
        q.eq("toUserId", me._id).eq("status", "pending"),
      )
      .take(50);

    const result = [];
    for (const request of requests) {
      const sender = await ctx.db.get("users", request.fromUserId);
      if (sender) {
        const presenceRow = await ctx.db
          .query("presence")
          .withIndex("by_userId", (q) => q.eq("userId", sender._id))
          .unique();
        const { lastSeenAt, presence } = effectivePresence(
          sender,
          presenceRow?.lastSeenAt ?? 0,
          false,
        );
        const avatarImageUrl = await avatarUrlFor(ctx, sender);
        const mutualServers = await mutualServerNames(
          ctx,
          me._id,
          sender._id,
          3,
        );
        result.push({
          requestId: request._id,
          fromUserId: sender._id,
          fromUsername: sender.username,
          fromDisplayName: sender.displayName,
          fromAvatarColor: sender.avatarColor ?? "",
          fromAvatarImageUrl: avatarImageUrl,
          fromStatusMessage: sender.statusMessage ?? "",
          note: request.note ?? "",
          sentAt: request.sentAt ?? request._creationTime,
          lastSeenAt,
          presence,
          mutualServers,
          isStaff: isStaff(sender),
        });
      }
    }
    result.sort((a, b) => b.sentAt - a.sentAt);
    return result;
  },
});

export const listOutgoingRequests = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const requests = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_status", (q) =>
        q.eq("fromUserId", me._id).eq("status", "pending"),
      )
      .take(50);

    const result = [];
    for (const request of requests) {
      const target = await ctx.db.get("users", request.toUserId);
      if (target) {
        const avatarImageUrl = await avatarUrlFor(ctx, target);
        result.push({
          requestId: request._id,
          toUserId: target._id,
          toUsername: target.username,
          toDisplayName: target.displayName,
          toAvatarColor: target.avatarColor ?? "",
          toAvatarImageUrl: avatarImageUrl,
          note: request.note ?? "",
          sentAt: request.sentAt ?? request._creationTime,
        });
      }
    }
    result.sort((a, b) => b.sentAt - a.sentAt);
    return result;
  },
});

export const countPendingIncoming = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const requests = await ctx.db
      .query("friendRequests")
      .withIndex("by_to_and_status", (q) =>
        q.eq("toUserId", me._id).eq("status", "pending"),
      )
      .take(100);
    return { count: requests.length };
  },
});

export const listFriends = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);

    const asSender = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_status", (q) =>
        q.eq("fromUserId", me._id).eq("status", "accepted"),
      )
      .take(200);
    const asReceiver = await ctx.db
      .query("friendRequests")
      .withIndex("by_to_and_status", (q) =>
        q.eq("toUserId", me._id).eq("status", "accepted"),
      )
      .take(200);

    type Pair = { friendId: Id<"users">; friendsSince: number };
    const pairs: Pair[] = [
      ...asSender.map((r) => ({
        friendId: r.toUserId,
        friendsSince: r.respondedAt ?? r._creationTime,
      })),
      ...asReceiver.map((r) => ({
        friendId: r.fromUserId,
        friendsSince: r.respondedAt ?? r._creationTime,
      })),
    ];

    const result = [];
    for (const { friendId, friendsSince } of pairs) {
      const friend = await ctx.db.get("users", friendId);
      if (!friend || friend.banned) continue;

      const presenceRow = await ctx.db
        .query("presence")
        .withIndex("by_userId", (q) => q.eq("userId", friend._id))
        .unique();
      const { lastSeenAt, presence } = effectivePresence(
        friend,
        presenceRow?.lastSeenAt ?? 0,
        false,
      );
      const avatarImageUrl = await avatarUrlFor(ctx, friend);
      const meta = await getMeta(ctx, me._id, friend._id);
      const mutualServers = await mutualServerNames(ctx, me._id, friend._id, 3);

      result.push({
        userId: friend._id,
        username: friend.username,
        displayName: friend.displayName,
        lastSeenAt,
        presence,
        avatarColor: friend.avatarColor ?? "",
        avatarImageUrl: avatarImageUrl ?? "",
        publicKey: friend.publicKey ?? "",
        statusMessage: friend.statusMessage ?? "",
        bio: friend.bio ?? "",
        nickname: meta?.nickname && meta.nickname.length > 0 ? meta.nickname : "",
        favorite: meta?.favorite === true,
        privateNote:
          meta?.privateNote && meta.privateNote.length > 0
            ? meta.privateNote
            : "",
        friendsSince,
        mutualServers,
        isStaff: isStaff(friend),
        platformRole: platformRole(friend),
      });
    }

    // Favorites → online-ish → alphabetical by effective display name.
    result.sort((a, b) => {
      if (a.favorite !== b.favorite) return a.favorite ? -1 : 1;
      const aOn = a.presence !== "offline" ? 1 : 0;
      const bOn = b.presence !== "offline" ? 1 : 0;
      if (aOn !== bOn) return bOn - aOn;
      const an = (a.nickname || a.displayName).toLowerCase();
      const bn = (b.nickname || b.displayName).toLowerCase();
      return an.localeCompare(bn);
    });

    return result;
  },
});

/** Dashboard counters for the social hub header. */
export const socialStats = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const friends = await friendshipIds(ctx, me._id);
    let online = 0;
    for (const id of friends) {
      const user = await ctx.db.get("users", id as Id<"users">);
      if (!user) continue;
      const presenceRow = await ctx.db
        .query("presence")
        .withIndex("by_userId", (q) => q.eq("userId", user._id))
        .unique();
      const { presence } = effectivePresence(
        user,
        presenceRow?.lastSeenAt ?? 0,
        false,
      );
      if (presence !== "offline") online += 1;
    }
    const incoming = await ctx.db
      .query("friendRequests")
      .withIndex("by_to_and_status", (q) =>
        q.eq("toUserId", me._id).eq("status", "pending"),
      )
      .take(100);
    const outgoing = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_status", (q) =>
        q.eq("fromUserId", me._id).eq("status", "pending"),
      )
      .take(100);
    return {
      friendsTotal: friends.size,
      friendsOnline: online,
      incomingPending: incoming.length,
      outgoingPending: outgoing.length,
    };
  },
});

/**
 * Search people by username / display name.
 * Discoverable users match prefix/substring; exact username always works.
 */
export const searchPeople = query({
  args: {
    sessionToken: v.string(),
    query: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const q = args.query.trim().toLowerCase().replace(/^@/, "");
    if (q.length < 2) return [];

    const friendIds = await friendshipIds(ctx, me._id);
    const users = await ctx.db.query("users").take(400);
    const outgoing = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_status", (q2) =>
        q2.eq("fromUserId", me._id).eq("status", "pending"),
      )
      .take(100);
    const incoming = await ctx.db
      .query("friendRequests")
      .withIndex("by_to_and_status", (q2) =>
        q2.eq("toUserId", me._id).eq("status", "pending"),
      )
      .take(100);
    const pendingOut = new Set(outgoing.map((r) => r.toUserId as string));
    const pendingIn = new Map(
      incoming.map((r) => [r.fromUserId as string, r._id as string]),
    );

    const results = [];
    for (const user of users) {
      if (user._id === me._id || user.isBot || user.banned) continue;
      const exact = user.username === q;
      const discoverable = user.discoverable !== false;
      if (!exact && !discoverable) continue;
      const uname = user.username.toLowerCase();
      const dname = user.displayName.toLowerCase();
      if (!exact && !uname.includes(q) && !dname.includes(q)) continue;
      if (await isBlockedEitherWay(ctx, me._id, user._id)) continue;

      const presenceRow = await ctx.db
        .query("presence")
        .withIndex("by_userId", (q2) => q2.eq("userId", user._id))
        .unique();
      const { lastSeenAt, presence } = effectivePresence(
        user,
        presenceRow?.lastSeenAt ?? 0,
        false,
      );
      const mutualServers = await mutualServerNames(ctx, me._id, user._id, 2);

      let relation: "none" | "friends" | "outgoing" | "incoming" = "none";
      if (friendIds.has(user._id)) relation = "friends";
      else if (pendingOut.has(user._id)) relation = "outgoing";
      else if (pendingIn.has(user._id)) relation = "incoming";

      results.push({
        userId: user._id,
        username: user.username,
        displayName: user.displayName,
        avatarColor: user.avatarColor ?? "",
        avatarImageUrl: await avatarUrlFor(ctx, user),
        statusMessage: user.statusMessage ?? "",
        lastSeenAt,
        presence,
        relation,
        incomingRequestId: pendingIn.get(user._id) ?? "",
        mutualServers,
        isStaff: isStaff(user),
        score: exact ? 0 : uname.startsWith(q) ? 1 : 2,
      });
      if (results.length >= 30) break;
    }

    results.sort((a, b) => a.score - b.score || a.username.localeCompare(b.username));
    return results.slice(0, 20);
  },
});

/** People from mutual servers who are not friends yet. */
export const suggestPeople = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const friendIds = await friendshipIds(ctx, me._id);
    const myServers = await ctx.db
      .query("serverMembers")
      .withIndex("by_user", (q) => q.eq("userId", me._id))
      .take(50);

    const scores = new Map<string, { userId: Id<"users">; count: number; names: string[] }>();
    for (const mem of myServers) {
      const members = await ctx.db
        .query("serverMembers")
        .withIndex("by_server", (q) => q.eq("serverId", mem.serverId))
        .take(80);
      const server = await ctx.db.get("servers", mem.serverId);
      const serverName = server?.name ?? "Server";
      for (const m of members) {
        if (m.userId === me._id) continue;
        if (friendIds.has(m.userId)) continue;
        const key = m.userId as string;
        const cur = scores.get(key) ?? {
          userId: m.userId,
          count: 0,
          names: [] as string[],
        };
        cur.count += 1;
        if (cur.names.length < 3 && !cur.names.includes(serverName)) {
          cur.names.push(serverName);
        }
        scores.set(key, cur);
      }
    }

    const ranked = [...scores.values()].sort((a, b) => b.count - a.count);
    const result = [];
    for (const entry of ranked) {
      if (result.length >= 15) break;
      const user = await ctx.db.get("users", entry.userId);
      if (!user || user.isBot || user.banned) continue;
      if (await isBlockedEitherWay(ctx, me._id, user._id)) continue;
      if (privacyOf(user) === "nobody") continue;

      const presenceRow = await ctx.db
        .query("presence")
        .withIndex("by_userId", (q) => q.eq("userId", user._id))
        .unique();
      const { lastSeenAt, presence } = effectivePresence(
        user,
        presenceRow?.lastSeenAt ?? 0,
        false,
      );

      result.push({
        userId: user._id,
        username: user.username,
        displayName: user.displayName,
        avatarColor: user.avatarColor ?? "",
        avatarImageUrl: await avatarUrlFor(ctx, user),
        statusMessage: user.statusMessage ?? "",
        lastSeenAt,
        presence,
        mutualServers: entry.names,
        mutualCount: entry.count,
      });
    }
    return result;
  },
});

/** Relationship summary for a profile card. */
export const getRelationship = query({
  args: {
    sessionToken: v.string(),
    userId: v.id("users"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (args.userId === me._id) {
      return {
        relation: "self" as const,
        requestId: "",
        canSupportDm: false,
        mutualServers: [] as string[],
      };
    }
    const other = await ctx.db.get("users", args.userId);
    if (!other) throw new Error("User not found");

    const forward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", me._id).eq("toUserId", args.userId),
      )
      .unique();
    const backward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", args.userId).eq("toUserId", me._id),
      )
      .unique();

    let relation:
      | "none"
      | "friends"
      | "outgoing"
      | "incoming"
      | "blocked" = "none";
    let requestId = "";
    if (await isBlockedEitherWay(ctx, me._id, args.userId)) {
      relation = "blocked";
    } else if (
      forward?.status === "accepted" ||
      backward?.status === "accepted"
    ) {
      relation = "friends";
    } else if (forward?.status === "pending") {
      relation = "outgoing";
      requestId = forward._id;
    } else if (backward?.status === "pending") {
      relation = "incoming";
      requestId = backward._id;
    }

    const meta =
      relation === "friends" ? await getMeta(ctx, me._id, args.userId) : null;

    return {
      relation,
      requestId,
      canSupportDm: isStaff(me) || isStaff(other),
      mutualServers: await mutualServerNames(ctx, me._id, args.userId, 5),
      favorite: meta?.favorite === true,
      nickname: meta?.nickname ?? "",
      privateNote: meta?.privateNote ?? "",
    };
  },
});

// ─── Maintenance ─────────────────────────────────────────────────────────────

export const cleanupStaleDeclined = internalMutation({
  args: {},
  handler: async (ctx) => {
    const cutoff = Date.now() - DECLINED_RETENTION_MS;
    const declined = await ctx.db
      .query("friendRequests")
      .withIndex("by_status", (q) => q.eq("status", "declined"))
      .take(500);

    let deleted = 0;
    for (const row of declined) {
      const stamp = row.respondedAt ?? row._creationTime;
      if (stamp < cutoff) {
        await ctx.db.delete("friendRequests", row._id);
        deleted += 1;
      }
    }
    return { deleted, scanned: declined.length };
  },
});

export const purgeAutoAcceptedFriendships = internalMutation({
  args: {},
  handler: async (ctx) => {
    const accepted = await ctx.db
      .query("friendRequests")
      .withIndex("by_status", (q) => q.eq("status", "accepted"))
      .take(2000);

    let deleted = 0;
    let scanned = 0;
    for (const row of accepted) {
      scanned += 1;
      const from = await ctx.db.get("users", row.fromUserId);
      const to = await ctx.db.get("users", row.toUserId);
      const fromProtected = from ? isProtectedTarget(from) : false;
      const toProtected = to ? isProtectedTarget(to) : false;
      if (fromProtected || toProtected) {
        await ctx.db.delete("friendRequests", row._id);
        deleted += 1;
      }
    }
    return { deleted, scanned };
  },
});

export const purgeAutoAcceptedFriendshipsAsAdmin = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (!isProtectedTarget(me)) {
      throw new Error("Admin permission required");
    }

    const accepted = await ctx.db
      .query("friendRequests")
      .withIndex("by_status", (q) => q.eq("status", "accepted"))
      .take(2000);

    let deleted = 0;
    let scanned = 0;
    for (const row of accepted) {
      scanned += 1;
      const from = await ctx.db.get("users", row.fromUserId);
      const to = await ctx.db.get("users", row.toUserId);
      const fromProtected = from ? isProtectedTarget(from) : false;
      const toProtected = to ? isProtectedTarget(to) : false;
      if (fromProtected || toProtected) {
        await ctx.db.delete("friendRequests", row._id);
        deleted += 1;
      }
    }
    return { deleted, scanned };
  },
});
