import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser } from "./session";
import { Id } from "./_generated/dataModel";

async function requireMembership(
  ctx: QueryCtx | MutationCtx,
  conversationId: Id<"conversations">,
  userId: Id<"users">,
) {
  const membership = await ctx.db
    .query("conversationMembers")
    .withIndex("by_conversation_and_user", (q) =>
      q.eq("conversationId", conversationId).eq("userId", userId),
    )
    .unique();
  if (!membership) {
    throw new Error("You're not a member of this chat");
  }
}

/** Groups and server channels use shared conversation keys (not DMs). */
function supportsGroupKey(kind: string): boolean {
  return kind === "group" || kind === "channel";
}

/**
 * Public keys of every member (for sealing a new group key client-side).
 * Members without a published key are returned with publicKey="" so the
 * client can still bootstrap for the rest and share later.
 */
export const listMemberPublicKeys = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);

    const conversation = await ctx.db.get("conversations", args.conversationId);
    if (!conversation || !supportsGroupKey(conversation.kind)) {
      return { epoch: 0, members: [] as { userId: Id<"users">; publicKey: string }[] };
    }

    const members = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(200);

    const out: { userId: Id<"users">; publicKey: string }[] = [];
    for (const m of members) {
      const user = await ctx.db.get("users", m.userId);
      out.push({
        userId: m.userId,
        publicKey: user?.publicKey ?? "",
      });
    }
    return {
      epoch: conversation.keyEpoch ?? 0,
      members: out,
    };
  },
});

/** My sealed package for this conversation (latest epoch only). */
export const myPackage = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);

    const conversation = await ctx.db.get("conversations", args.conversationId);
    if (!conversation || !supportsGroupKey(conversation.kind)) {
      return null;
    }

    const row = await ctx.db
      .query("conversationKeyPackages")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (!row) return null;

    return {
      epoch: row.epoch,
      sealedKey: row.sealedKey,
      ephPublicKey: row.ephPublicKey,
      conversationEpoch: conversation.keyEpoch ?? row.epoch,
    };
  },
});

/**
 * Bootstrap or replace sealed packages for the current epoch.
 * Client generates the group key and seals it to each member offline;
 * Convex never sees the plaintext key.
 *
 * If packages already exist for this conversation and `force` is false,
 * the mutation is a no-op that returns the existing epoch (first writer wins).
 */
export const publishPackages = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    epoch: v.number(),
    force: v.optional(v.boolean()),
    packages: v.array(
      v.object({
        userId: v.id("users"),
        sealedKey: v.string(),
        ephPublicKey: v.string(),
      }),
    ),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);

    const conversation = await ctx.db.get("conversations", args.conversationId);
    if (!conversation || !supportsGroupKey(conversation.kind)) {
      throw new Error("Group keys only apply to groups and channels");
    }
    if (args.epoch < 1 || args.epoch > 1_000_000) {
      throw new Error("Invalid key epoch");
    }
    if (args.packages.length === 0) {
      throw new Error("No key packages");
    }
    if (args.packages.length > 200) {
      throw new Error("Too many key packages");
    }

    const existingEpoch = conversation.keyEpoch ?? 0;
    const force = args.force === true;

    if (existingEpoch > 0 && !force) {
      // Another member already bootstrapped — client should fetch myPackage.
      return { epoch: existingEpoch, created: false };
    }
    if (force && args.epoch <= existingEpoch) {
      throw new Error("New epoch must be greater than the current one");
    }
    if (!force && args.epoch !== 1 && existingEpoch === 0) {
      // First bootstrap must start at epoch 1.
      if (args.epoch !== 1) {
        throw new Error("First group key epoch must be 1");
      }
    }

    const memberRows = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(200);
    const memberSet = new Set(memberRows.map((m) => m.userId));

    for (const pkg of args.packages) {
      if (!memberSet.has(pkg.userId)) {
        throw new Error("Package targets a non-member");
      }
      if (pkg.sealedKey.length < 16 || pkg.sealedKey.length > 512) {
        throw new Error("Invalid sealed key");
      }
      if (pkg.ephPublicKey.length !== 44) {
        throw new Error("Invalid ephemeral public key");
      }
    }

    // Drop prior packages for this conversation (one package per user).
    const old = await ctx.db
      .query("conversationKeyPackages")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(500);
    for (const row of old) {
      await ctx.db.delete("conversationKeyPackages", row._id);
    }

    const now = Date.now();
    for (const pkg of args.packages) {
      await ctx.db.insert("conversationKeyPackages", {
        conversationId: args.conversationId,
        userId: pkg.userId,
        epoch: args.epoch,
        sealedKey: pkg.sealedKey,
        ephPublicKey: pkg.ephPublicKey,
        createdBy: me._id,
        createdAt: now,
      });
    }

    await ctx.db.patch("conversations", args.conversationId, {
      keyEpoch: args.epoch,
    });

    return { epoch: args.epoch, created: true };
  },
});

/**
 * Add sealed packages for members who joined after the key was created
 * (same epoch — no rotation). Caller must already hold the group key.
 */
export const shareWithMembers = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    packages: v.array(
      v.object({
        userId: v.id("users"),
        sealedKey: v.string(),
        ephPublicKey: v.string(),
      }),
    ),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);

    const conversation = await ctx.db.get("conversations", args.conversationId);
    if (!conversation || !supportsGroupKey(conversation.kind)) {
      throw new Error("Group keys only apply to groups and channels");
    }
    const epoch = conversation.keyEpoch ?? 0;
    if (epoch < 1) {
      throw new Error("No group key yet — bootstrap first");
    }

    // Caller must already have a package (proves they were trusted with the key).
    const mine = await ctx.db
      .query("conversationKeyPackages")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (!mine || mine.epoch !== epoch) {
      throw new Error("You don't have the current group key");
    }

    const memberRows = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(200);
    const memberSet = new Set(memberRows.map((m) => m.userId));
    const now = Date.now();
    let shared = 0;

    for (const pkg of args.packages) {
      if (!memberSet.has(pkg.userId)) continue;
      if (pkg.ephPublicKey.length !== 44) continue;
      if (pkg.sealedKey.length < 16 || pkg.sealedKey.length > 512) continue;

      const existing = await ctx.db
        .query("conversationKeyPackages")
        .withIndex("by_conversation_and_user", (q) =>
          q.eq("conversationId", args.conversationId).eq("userId", pkg.userId),
        )
        .unique();
      if (existing) {
        if (existing.epoch === epoch) continue;
        await ctx.db.patch("conversationKeyPackages", existing._id, {
          epoch,
          sealedKey: pkg.sealedKey,
          ephPublicKey: pkg.ephPublicKey,
          createdBy: me._id,
          createdAt: now,
        });
      } else {
        await ctx.db.insert("conversationKeyPackages", {
          conversationId: args.conversationId,
          userId: pkg.userId,
          epoch,
          sealedKey: pkg.sealedKey,
          ephPublicKey: pkg.ephPublicKey,
          createdBy: me._id,
          createdAt: now,
        });
      }
      shared += 1;
    }

    return { shared, epoch };
  },
});
