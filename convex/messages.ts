import { v } from "convex/values";
import { mutation, internalMutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser } from "./session";
import { Id } from "./_generated/dataModel";
import { conversationAllowsStorage } from "./prefs";

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

export const REACTION_EMOJIS = ["👍", "❤️", "😂", "😮", "😢", "🎉"];

export const list = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);

    const isAdmin = me.role === "admin";
    const messages = await ctx.db
      .query("messages")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .order("desc")
      .take(100);

    // Avatar images are looked up live (not denormalized onto the message)
    // so a profile photo change shows up on the sender's older messages too.
    // Cache per-author within this page since one author usually sent
    // several of the messages being returned.
    const authorMeta = new Map<string, { url: string; isBot: boolean }>();
    async function resolveAuthor(authorId: Id<"users">) {
      const cached = authorMeta.get(authorId);
      if (cached !== undefined) return cached;
      const author = await ctx.db.get("users", authorId);
      const url = author?.avatarStorageId
        ? ((await ctx.storage.getUrl(author.avatarStorageId)) ?? "")
        : "";
      const meta = { url, isBot: author?.isBot === true };
      authorMeta.set(authorId, meta);
      return meta;
    }

    return Promise.all(
      messages.reverse().map(async (message) => {
        const deleted = message.deleted ?? false;
        const canSeeReal = isAdmin || message.authorId === me._id || !deleted;

        let attachmentUrl = "";
        if (canSeeReal && message.attachmentStorageId) {
          attachmentUrl =
            (await ctx.storage.getUrl(message.attachmentStorageId)) ?? "";
        }

        let reactions: { emoji: string; count: number; reactedByMe: boolean }[] =
          [];
        if (canSeeReal) {
          const rows = await ctx.db
            .query("reactions")
            .withIndex("by_message", (q) => q.eq("messageId", message._id))
            .take(200);
          const byEmoji = new Map<string, { count: number; reactedByMe: boolean }>();
          for (const row of rows) {
            const entry = byEmoji.get(row.emoji) ?? { count: 0, reactedByMe: false };
            entry.count += 1;
            if (row.userId === me._id) {
              entry.reactedByMe = true;
            }
            byEmoji.set(row.emoji, entry);
          }
          reactions = Array.from(byEmoji.entries()).map(([emoji, entry]) => ({
            emoji,
            ...entry,
          }));
        }

        let replyTo: {
          authorName: string;
          snippet: string;
          encrypted: boolean;
        } | null = null;
        if (canSeeReal && message.replyToMessageId) {
          const target = await ctx.db.get("messages", message.replyToMessageId);
          if (target) {
            const targetDeleted = target.deleted ?? false;
            const targetEncrypted = target.encrypted ?? false;
            // Encrypted bodies are opaque ciphertext blobs -- truncating
            // one server-side would just corrupt it. The full blob goes
            // out instead, and the client decrypts + truncates it.
            const snippet = targetDeleted
              ? "Message deleted"
              : targetEncrypted
                ? target.body
                : target.body.length > 80
                  ? `${target.body.slice(0, 80)}...`
                  : target.body;
            replyTo = {
              authorName: target.authorName,
              snippet,
              encrypted: !targetDeleted && targetEncrypted,
            };
          }
        }

        const author = await resolveAuthor(message.authorId);
        return {
          id: message._id,
          authorId: message.authorId,
          authorName: message.authorName,
          authorAvatarColor: message.authorAvatarColor ?? "",
          authorAvatarImageUrl: author.url,
          authorIsBot: author.isBot,
          body: canSeeReal ? message.body : "Message deleted",
          kind: message.kind ?? "text",
          encrypted: canSeeReal && (message.encrypted ?? false),
          attachmentUrl,
          reactions,
          replyTo,
          deleted,
          edited: message.editedAt !== undefined,
          sentAt: message._creationTime,
        };
      }),
    );
  },
});

export const toggleReaction = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
    emoji: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (!REACTION_EMOJIS.includes(args.emoji)) {
      throw new Error("Unsupported reaction");
    }
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    await requireMembership(ctx, message.conversationId, me._id);

    const existing = await ctx.db
      .query("reactions")
      .withIndex("by_message_and_user_and_emoji", (q) =>
        q
          .eq("messageId", args.messageId)
          .eq("userId", me._id)
          .eq("emoji", args.emoji),
      )
      .unique();
    if (existing) {
      await ctx.db.delete("reactions", existing._id);
    } else {
      await ctx.db.insert("reactions", {
        messageId: args.messageId,
        userId: me._id,
        emoji: args.emoji,
      });
    }
    return null;
  },
});

const MAX_ATTACHMENT_BYTES = 5 * 1024 * 1024;

export const generateAttachmentUploadUrl = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    await currentUser(ctx, args.sessionToken);
    return await ctx.storage.generateUploadUrl();
  },
});

// Encrypted bodies are base64 ratchet blobs (header + GCM ciphertext), which
// run longer than the plaintext they hold -- give them more headroom than
// the plain-text cap used for groups/channels.
const MAX_ENCRYPTED_BODY_LENGTH = 48_000;
const MAX_PLAINTEXT_BODY_LENGTH = 4000;

export const send = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    body: v.string(),
    attachmentStorageId: v.optional(v.id("_storage")),
    replyToMessageId: v.optional(v.id("messages")),
    encrypted: v.optional(v.boolean()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);

    const conversation = await ctx.db.get("conversations", args.conversationId);
    if (!conversation) {
      throw new Error("Conversation not found");
    }

    // DMs: live traffic is peerseal; optional encrypted body is legacy TKR3
    // or plaintext durable history. Groups/channels: encrypted bodies use
    // TGK1 conversation keys (see groupKeys.ts / crypto.rs).
    const kind = conversation.kind;
    if (args.encrypted && kind !== "direct" && kind !== "group" && kind !== "channel") {
      throw new Error("This chat type does not support encrypted messages");
    }

    // Encrypted bodies are opaque ciphertext -- trimming whitespace off an
    // encrypted blob would corrupt it, and an empty check doesn't mean
    // anything for one either (a base64 blob is never actually empty).
    const body = args.encrypted ? args.body : args.body.trim();
    if (!args.encrypted && body.length === 0 && !args.attachmentStorageId) {
      return null;
    }
    if (args.encrypted && body.length === 0) {
      throw new Error("Encrypted message body is required");
    }
    const maxLength = args.encrypted
      ? MAX_ENCRYPTED_BODY_LENGTH
      : MAX_PLAINTEXT_BODY_LENGTH;
    if (body.length > maxLength) {
      throw new Error("Message is too long");
    }

    if (args.attachmentStorageId) {
      const metadata = await ctx.db.system.get(
        "_storage",
        args.attachmentStorageId,
      );
      if (!metadata || metadata.size > MAX_ATTACHMENT_BYTES) {
        await ctx.storage.delete(args.attachmentStorageId);
        throw new Error("Attachment must be smaller than 5MB");
      }
    }

    // Ephemeral: any member opted out of storage (global or per-chat).
    if (!(await conversationAllowsStorage(ctx, args.conversationId))) {
      if (args.attachmentStorageId) {
        try {
          await ctx.storage.delete(args.attachmentStorageId);
        } catch {
          /* ignore */
        }
      }
      return { stored: false };
    }

    let replyToMessageId = args.replyToMessageId;
    if (replyToMessageId) {
      const target = await ctx.db.get("messages", replyToMessageId);
      if (!target || target.conversationId !== args.conversationId) {
        replyToMessageId = undefined;
      }
    }

    await ctx.db.insert("messages", {
      conversationId: args.conversationId,
      authorId: me._id,
      authorName: me.displayName,
      authorAvatarColor: me.avatarColor,
      body,
      attachmentStorageId: args.attachmentStorageId,
      replyToMessageId,
      encrypted: args.encrypted ? true : undefined,
    });
    await ctx.db.patch("conversations", args.conversationId, {
      lastMessageAt: Date.now(),
    });
    return { stored: true };
  },
});

export const edit = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
    body: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    if (message.kind === "call") {
      throw new Error("Call history can't be edited");
    }
    if (message.authorId !== me._id) {
      throw new Error("You can only edit your own messages");
    }
    if (message.deleted) {
      throw new Error("You can't edit a deleted message");
    }

    // Whether this message is encrypted is fixed at send time and never
    // changes; the client is responsible for re-encrypting an edit to a
    // message that was originally encrypted, same as it does when sending.
    const isEncrypted = message.encrypted ?? false;
    const body = isEncrypted ? args.body : args.body.trim();
    if (!isEncrypted && body.length === 0) {
      throw new Error("Message can't be empty");
    }
    const maxLength = isEncrypted
      ? MAX_ENCRYPTED_BODY_LENGTH
      : MAX_PLAINTEXT_BODY_LENGTH;
    if (body.length > maxLength) {
      throw new Error("Message is too long");
    }

    await ctx.db.patch("messages", message._id, {
      body,
      editedAt: Date.now(),
    });
    return null;
  },
});

export const remove = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    const isAdmin = me.role === "admin";
    if (message.kind === "call") {
      if (!isAdmin) {
        throw new Error("Only an admin can remove call history");
      }
    } else if (message.authorId !== me._id && !isAdmin) {
      throw new Error("You don't have permission to delete this message");
    }

    await ctx.db.patch("messages", message._id, {
      deleted: true,
      deletedAt: Date.now(),
      deletedBy: me._id,
    });
    return null;
  },
});

export const purge = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (me.role !== "admin") {
      throw new Error("Only an admin can permanently delete a message");
    }
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      return null;
    }
    if (message.attachmentStorageId) {
      await ctx.storage.delete(message.attachmentStorageId);
    }
    const reactionRows = await ctx.db
      .query("reactions")
      .withIndex("by_message", (q) => q.eq("messageId", message._id))
      .take(200);
    for (const row of reactionRows) {
      await ctx.db.delete("reactions", row._id);
    }
    await ctx.db.delete("messages", message._id);
    return null;
  },
});

/**
 * Hard-delete every message in one conversation (plus reactions / attachment
 * blobs) for all members. Used by the client "Clear chat" action so both
 * sides stop seeing Convex-backed history. Call repeatedly until
 * `done: true` if a channel has a huge backlog.
 */
export const clearConversation = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);

    const batch = await ctx.db
      .query("messages")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(400);

    let purged = 0;
    for (const message of batch) {
      if (message.attachmentStorageId) {
        try {
          await ctx.storage.delete(message.attachmentStorageId);
        } catch {
          // Storage object may already be gone.
        }
      }
      const reactionRows = await ctx.db
        .query("reactions")
        .withIndex("by_message", (q) => q.eq("messageId", message._id))
        .take(200);
      for (const row of reactionRows) {
        await ctx.db.delete("reactions", row._id);
      }
      await ctx.db.delete("messages", message._id);
      purged += 1;
    }

    if (batch.length < 400) {
      await ctx.db.patch("conversations", args.conversationId, {
        lastMessageAt: undefined,
      });
    }

    return { purged, done: batch.length < 400 };
  },
});

async function purgeMessageBatch(ctx: MutationCtx) {
  const batch = await ctx.db.query("messages").take(400);
  let purged = 0;
  for (const message of batch) {
    if (message.attachmentStorageId) {
      try {
        await ctx.storage.delete(message.attachmentStorageId);
      } catch {
        // Storage object may already be gone; still drop the message row.
      }
    }
    const reactionRows = await ctx.db
      .query("reactions")
      .withIndex("by_message", (q) => q.eq("messageId", message._id))
      .take(200);
    for (const row of reactionRows) {
      await ctx.db.delete("reactions", row._id);
    }
    await ctx.db.delete("messages", message._id);
    purged += 1;
  }

  // Clear last-message timestamps so sidebars don't show stale previews.
  if (purged > 0) {
    const conversations = await ctx.db.query("conversations").take(200);
    for (const conversation of conversations) {
      if (conversation.lastMessageAt !== undefined) {
        await ctx.db.patch("conversations", conversation._id, {
          lastMessageAt: undefined,
        });
      }
    }
  }

  return { purged, done: batch.length < 400 };
}

/**
 * One-shot wipe of every message (and its reactions / attachment blobs) so
 * a crypto protocol upgrade can start from an empty history. Admin-only.
 * Processes up to 400 messages per call -- invoke repeatedly until
 * `done: true` if the deployment has a large backlog.
 */
export const purgeAllHistory = mutation({
  args: {
    sessionToken: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (me.role !== "admin") {
      throw new Error("Only an admin can wipe chat history");
    }
    return await purgeMessageBatch(ctx);
  },
});

/**
 * CLI-only wipe for the E2EE protocol upgrade. Marked `internalMutation` so
 * it is NOT reachable from the public client API (a public mutation with only
 * a literal confirm string would let anyone wipe all history). Invoke it from
 * the CLI as an internal function:
 *
 *   npx convex run internal:messages:purgeAllHistoryConfirm '{"confirm":"WIPE_ALL_MESSAGES"}'
 */
export const purgeAllHistoryConfirm = internalMutation({
  args: {
    confirm: v.literal("WIPE_ALL_MESSAGES"),
  },
  handler: async (ctx) => {
    return await purgeMessageBatch(ctx);
  },
});
