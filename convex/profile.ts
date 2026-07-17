import { v } from "convex/values";
import { mutation, query } from "./_generated/server";
import {
  currentUser,
  isBlockedEitherWay,
  isStaff,
  platformRole,
} from "./session";

const ONLINE_MS = 90_000;

// Must match `AVATAR_PALETTE` in src/main.rs (the client's swatch picker) --
// any mismatch means every profile save gets rejected here with "Invalid
// avatar color" the moment the color isn't already the user's current one.
export const AVATAR_PALETTE = [
  "#3FB36B",
  "#2E9E6B",
  "#7FCBA0",
  "#2F8F57",
  "#A9B85E",
  "#5FB98C",
  "#27814F",
  "#9FD3B5",
];

// Base64 X25519 public keys are always exactly 32 raw bytes -> 44 base64
// characters (with padding). A sanity check, not real validation of the
// point itself -- a malformed/invalid key just means ECDH with it will
// fail later, handled gracefully client-side.
const PUBLIC_KEY_BASE64_LENGTH = 44;

// Always overwrites rather than "set once": the matching private key lives
// only on the user's device, so losing it (reinstall, wiped profile
// folder, ...) has to be recoverable by generating a fresh keypair and
// re-publishing it -- otherwise a user who loses their key would be
// permanently locked out of sending encrypted messages. The tradeoff is
// that old messages encrypted to the previous key become undecryptable,
// which is the expected, unavoidable consequence of losing an E2EE key.
export const setPublicKey = mutation({
  args: {
    sessionToken: v.string(),
    publicKey: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (args.publicKey.length !== PUBLIC_KEY_BASE64_LENGTH) {
      throw new Error("Invalid public key");
    }
    await ctx.db.patch("users", me._id, { publicKey: args.publicKey });
    return null;
  },
});

export const updateProfile = mutation({
  args: {
    sessionToken: v.string(),
    displayName: v.string(),
    statusMessage: v.string(),
    bio: v.string(),
    avatarColor: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);

    const displayName = args.displayName.trim();
    if (displayName.length === 0) {
      throw new Error("Display name can't be empty");
    }
    if (displayName.length > 50) {
      throw new Error("Display name is too long");
    }
    if (!AVATAR_PALETTE.includes(args.avatarColor)) {
      throw new Error("Invalid avatar color");
    }

    const statusMessage = args.statusMessage.trim().slice(0, 100);
    const bio = args.bio.trim().slice(0, 300);

    await ctx.db.patch("users", me._id, {
      displayName,
      statusMessage: statusMessage.length > 0 ? statusMessage : undefined,
      bio: bio.length > 0 ? bio : undefined,
      avatarColor: args.avatarColor,
    });
    return null;
  },
});

export const getProfile = query({
  args: {
    sessionToken: v.string(),
    userId: v.id("users"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const user = await ctx.db.get("users", args.userId);
    if (!user) {
      throw new Error("User not found");
    }
    if (await isBlockedEitherWay(ctx, me._id, args.userId)) {
      throw new Error("You can't view this profile");
    }

    const presenceRow = await ctx.db
      .query("presence")
      .withIndex("by_userId", (q) => q.eq("userId", user._id))
      .unique();
    const avatarImageUrl = user.avatarStorageId
      ? await ctx.storage.getUrl(user.avatarStorageId)
      : null;

    const forward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", me._id).eq("toUserId", user._id),
      )
      .unique();
    const backward = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_to", (q) =>
        q.eq("fromUserId", user._id).eq("toUserId", me._id),
      )
      .unique();
    const isFriend =
      forward?.status === "accepted" || backward?.status === "accepted";

    let relation: "self" | "none" | "friends" | "outgoing" | "incoming" = "none";
    let requestId = "";
    if (user._id === me._id) {
      relation = "self";
    } else if (isFriend) {
      relation = "friends";
    } else if (forward?.status === "pending") {
      relation = "outgoing";
      requestId = forward._id;
    } else if (backward?.status === "pending") {
      relation = "incoming";
      requestId = backward._id;
    }

    const isSelf = user._id === me._id;
    let lastSeenAt = presenceRow?.lastSeenAt ?? 0;
    let presenceLabel = "offline";
    const preferred = user.presenceStatus ?? "online";
    if (
      !isSelf &&
      (user.hideOnlineStatus === true || preferred === "invisible")
    ) {
      lastSeenAt = 0;
      presenceLabel = "offline";
    } else if (lastSeenAt > 0 && Date.now() - lastSeenAt < ONLINE_MS) {
      presenceLabel =
        preferred === "idle" || preferred === "dnd" ? preferred : "online";
    }

    // Mutual servers (max 5 names).
    const myServers = await ctx.db
      .query("serverMembers")
      .withIndex("by_user", (q) => q.eq("userId", me._id))
      .take(100);
    const mutualServers: string[] = [];
    if (!isSelf) {
      for (const m of myServers) {
        if (mutualServers.length >= 5) break;
        const other = await ctx.db
          .query("serverMembers")
          .withIndex("by_server_and_user", (q) =>
            q.eq("serverId", m.serverId).eq("userId", user._id),
          )
          .unique();
        if (!other) continue;
        const server = await ctx.db.get("servers", m.serverId);
        if (server) mutualServers.push(server.name);
      }
    }

    const meta = isFriend
      ? await ctx.db
          .query("friendMeta")
          .withIndex("by_owner_and_friend", (q) =>
            q.eq("ownerId", me._id).eq("friendId", user._id),
          )
          .unique()
      : null;

    return {
      userId: user._id,
      username: user.username,
      displayName: user.displayName,
      avatarColor: user.avatarColor ?? "",
      avatarImageUrl: avatarImageUrl ?? "",
      statusMessage: user.statusMessage ?? "",
      bio: user.bio ?? "",
      lastSeenAt,
      presence: presenceLabel,
      isStaff: isStaff(user),
      platformRole: platformRole(user),
      isFriend,
      relation,
      requestId,
      canSupportDm: isStaff(me) || isStaff(user),
      mutualServers,
      favorite: meta?.favorite === true,
      nickname: meta?.nickname ?? "",
      privateNote: meta?.privateNote ?? "",
    };
  },
});

const MAX_AVATAR_BYTES = 2 * 1024 * 1024;

export const generateAvatarUploadUrl = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    await currentUser(ctx, args.sessionToken);
    return await ctx.storage.generateUploadUrl();
  },
});

export const setAvatarImage = mutation({
  args: {
    sessionToken: v.string(),
    storageId: v.id("_storage"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);

    const metadata = await ctx.db.system.get("_storage", args.storageId);
    if (!metadata || metadata.size > MAX_AVATAR_BYTES) {
      await ctx.storage.delete(args.storageId);
      throw new Error("Image must be smaller than 2MB");
    }

    const previousId = me.avatarStorageId;
    await ctx.db.patch("users", me._id, { avatarStorageId: args.storageId });
    if (previousId && previousId !== args.storageId) {
      await ctx.storage.delete(previousId);
    }

    const url = await ctx.storage.getUrl(args.storageId);
    return url ?? "";
  },
});

export const removeAvatarImage = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (me.avatarStorageId) {
      await ctx.storage.delete(me.avatarStorageId);
      await ctx.db.patch("users", me._id, { avatarStorageId: undefined });
    }
    return null;
  },
});
