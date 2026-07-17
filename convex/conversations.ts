import { v } from "convex/values";
import { mutation, query, MutationCtx } from "./_generated/server";
import {
  currentUser,
  isBlockedEitherWay,
  isStaff,
} from "./session";
import { Id } from "./_generated/dataModel";

function directKeyFor(a: Id<"users">, b: Id<"users">): string {
  return [a, b].sort().join("_");
}

async function ensureDirectConversation(
  ctx: MutationCtx,
  meId: Id<"users">,
  otherUserId: Id<"users">,
): Promise<Id<"conversations">> {
  const directKey = directKeyFor(meId, otherUserId);
  const existing = await ctx.db
    .query("conversations")
    .withIndex("by_directKey", (q) => q.eq("directKey", directKey))
    .unique();
  if (existing) {
    return existing._id;
  }

  const conversationId = await ctx.db.insert("conversations", {
    kind: "direct",
    directKey,
    createdBy: meId,
  });
  await ctx.db.insert("conversationMembers", {
    conversationId,
    userId: meId,
  });
  await ctx.db.insert("conversationMembers", {
    conversationId,
    userId: otherUserId,
  });
  return conversationId;
}

export const getOrCreateDirect = mutation({
  args: { sessionToken: v.string(), friendUserId: v.id("users") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (args.friendUserId === me._id) {
      throw new Error("You can't start a chat with yourself");
    }

    if (await isBlockedEitherWay(ctx, me._id, args.friendUserId)) {
      throw new Error("You can't message this user");
    }

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
    const isFriend =
      forward?.status === "accepted" || backward?.status === "accepted";
    if (!isFriend) {
      throw new Error("You can only message friends");
    }

    return await ensureDirectConversation(ctx, me._id, args.friendUserId);
  },
});

/**
 * Open a support DM without friendship.
 * Allowed when either side is Talkyss staff (moderator+).
 * Bypasses friends-only DMs so users can always reach staff.
 */
export const openSupportDm = mutation({
  args: {
    sessionToken: v.string(),
    /** The other participant (staff or user). */
    peerUserId: v.id("users"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (args.peerUserId === me._id) {
      throw new Error("You can't start a chat with yourself");
    }

    const other = await ctx.db.get("users", args.peerUserId);
    if (!other || other.isBot) {
      throw new Error("User not found");
    }

    const meStaff = isStaff(me);
    const otherStaff = isStaff(other);
    if (!meStaff && !otherStaff) {
      throw new Error("Support DMs require Talkyss staff on one side");
    }

    // Real blocks still apply unless the other party is protected staff
    // (isBlockedEitherWay already ignores blocks involving protected users).
    if (await isBlockedEitherWay(ctx, me._id, other._id)) {
      throw new Error("You can't message this user");
    }

    return await ensureDirectConversation(ctx, me._id, other._id);
  },
});

export const createGroup = mutation({
  args: {
    sessionToken: v.string(),
    name: v.string(),
    memberUserIds: v.array(v.id("users")),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const name = args.name.trim();
    if (name.length === 0) {
      throw new Error("Enter a group name");
    }

    const uniqueMemberIds = [...new Set(args.memberUserIds)].filter(
      (id) => id !== me._id,
    );
    if (uniqueMemberIds.length === 0) {
      throw new Error("Select at least one friend");
    }

    for (const memberId of uniqueMemberIds) {
      const forward = await ctx.db
        .query("friendRequests")
        .withIndex("by_from_and_to", (q) =>
          q.eq("fromUserId", me._id).eq("toUserId", memberId),
        )
        .unique();
      const backward = await ctx.db
        .query("friendRequests")
        .withIndex("by_from_and_to", (q) =>
          q.eq("fromUserId", memberId).eq("toUserId", me._id),
        )
        .unique();
      const isFriend =
        forward?.status === "accepted" || backward?.status === "accepted";
      if (!isFriend) {
        throw new Error("You can only add friends to a group");
      }
    }

    const conversationId = await ctx.db.insert("conversations", {
      kind: "group",
      name,
      createdBy: me._id,
    });

    await ctx.db.insert("conversationMembers", { conversationId, userId: me._id });
    for (const memberId of uniqueMemberIds) {
      await ctx.db.insert("conversationMembers", { conversationId, userId: memberId });
    }

    return conversationId;
  },
});

export const markRead = mutation({
  args: { sessionToken: v.string(), conversationId: v.id("conversations") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const membership = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (membership) {
      await ctx.db.patch("conversationMembers", membership._id, {
        lastReadAt: Date.now(),
      });
    }
    return null;
  },
});

export const listMyConversations = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);

    const memberships = await ctx.db
      .query("conversationMembers")
      .withIndex("by_user", (q) => q.eq("userId", me._id))
      .take(200);

    const result = [];
    for (const membership of memberships) {
      const conversation = await ctx.db.get(
        "conversations",
        membership.conversationId,
      );
      // Server channels have their own listing (servers:listChannels) and
      // their own tab in the UI -- keep them out of the plain chats list.
      if (!conversation || conversation.kind === "channel") continue;

      let title = conversation.name ?? "Chat";
      let peerUserId: Id<"users"> | null = null;
      if (conversation.kind === "direct") {
        const otherMembers = await ctx.db
          .query("conversationMembers")
          .withIndex("by_conversation", (q) =>
            q.eq("conversationId", conversation._id),
          )
          .take(2);
        const peerMembership = otherMembers.find((m) => m.userId !== me._id);
        if (peerMembership) {
          const peer = await ctx.db.get("users", peerMembership.userId);
          if (peer) {
            title = peer.displayName;
            peerUserId = peer._id;
          }
        }
      }

      const lastMessageAt = conversation.lastMessageAt ?? 0;
      const lastReadAt = membership.lastReadAt ?? 0;

      result.push({
        conversationId: conversation._id,
        kind: conversation.kind,
        title,
        peerUserId,
        lastMessageAt,
        unread: lastMessageAt > lastReadAt,
      });
    }

    result.sort((a, b) => b.lastMessageAt - a.lastMessageAt);
    return result;
  },
});
