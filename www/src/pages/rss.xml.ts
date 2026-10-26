import type { APIRoute } from "astro";
import { posts } from "../data/posts";

function escape(text: string): string {
  return text
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

export const GET: APIRoute = ({ site }) => {
  const origin = (site ?? new URL("https://shuru.run")).origin;
  const sorted = [...posts].sort((a, b) => (a.date < b.date ? 1 : -1));

  const items = sorted
    .map((post) => {
      const url = `${origin}/blog/${post.slug}`;
      const pubDate = new Date(`${post.date}T00:00:00Z`).toUTCString();
      return [
        "    <item>",
        `      <title>${escape(`${post.title} ${post.subtitle}`)}</title>`,
        `      <link>${url}</link>`,
        `      <guid isPermaLink="true">${url}</guid>`,
        `      <pubDate>${pubDate}</pubDate>`,
        `      <description>${escape(post.description)}</description>`,
        "    </item>",
      ].join("\n");
    })
    .join("\n");

  const xml = [
    '<?xml version="1.0" encoding="UTF-8"?>',
    '<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">',
    "  <channel>",
    "    <title>shuru blog</title>",
    `    <link>${origin}/blog</link>`,
    `    <atom:link href="${origin}/rss.xml" rel="self" type="application/rss+xml" />`,
    "    <description>Release notes and design notes for shuru, the local-first microVM sandbox for AI agents.</description>",
    "    <language>en</language>",
    items,
    "  </channel>",
    "</rss>",
    "",
  ].join("\n");

  return new Response(xml, {
    headers: { "Content-Type": "application/rss+xml; charset=utf-8" },
  });
};
