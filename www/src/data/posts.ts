export interface PostLink {
  label: string;
  href: string;
}

export interface Post {
  slug: string;
  title: string;
  subtitle: string;
  description: string;
  date: string;
  links: PostLink[];
}

export const posts: Post[] = [
  {
    slug: "recipes-docker-and-a-headless-browser",
    title: "Recipes, Docker in the VM,",
    subtitle: "and a headless browser.",
    description:
      "Shuru 0.8.0 adds guest services, recipes that build checkpoints from a versioned description, Docker Engine inside the VM, headless Chromium for browser agents, and checkpoint update.",
    date: "2026-10-26",
    links: [
      { label: "Changelog", href: "https://github.com/superhq-ai/shuru/blob/main/CHANGELOG.md" },
      { label: "Recipe catalog", href: "https://github.com/superhq-ai/recipes" },
      { label: "GitHub release", href: "https://github.com/superhq-ai/shuru/releases/tag/v0.8.0" },
    ],
  },
  {
    slug: "experimental-linux-builds",
    title: "Shuru on Linux",
    subtitle: "is live for ARM64 hosts.",
    description:
      "Shuru now publishes experimental Linux ARM64 builds with the KVM backend. Setup details live on the Linux page.",
    date: "2026-04-15",
    links: [
      { label: "GitHub Releases", href: "https://github.com/superhq-ai/shuru/releases" },
      { label: "GitHub repository", href: "https://github.com/superhq-ai/shuru" },
    ],
  },
  {
    slug: "local-sandbox-and-execution-layer-for-autonomous-agents",
    title: "Local sandbox and execution layer",
    subtitle: "for autonomous agents on macOS.",
    description:
      "Shuru is a local execution layer for AI agent orchestrators. Ephemeral Linux VMs, secret isolation, and checkpoints for multi-agent workflows.",
    date: "2026-03-12",
    links: [
      { label: "CLI and TypeScript SDK", href: "https://www.npmjs.com/package/@superhq/shuru" },
      { label: "GitHub", href: "https://github.com/superhq-ai/shuru" },
    ],
  },
];

export function getPost(slug: string): Post {
  const post = posts.find((p) => p.slug === slug);
  if (!post) {
    throw new Error(`unknown post: ${slug}`);
  }
  return post;
}

export function formatDate(date: string): string {
  return new Date(`${date}T00:00:00Z`).toLocaleDateString("en-US", {
    year: "numeric",
    month: "long",
    day: "numeric",
    timeZone: "UTC",
  });
}
