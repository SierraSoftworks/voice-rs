import { defineUserConfig, PageHeader } from 'vuepress'
import { viteBundler } from "@vuepress/bundler-vite";
import { defaultTheme } from "@vuepress/theme-default";

function htmlDecode(input: string): string {
  return input.replace("&#39;", "'").replace("&amp;", "&").replace("&quot;", '"')
}

function fixPageHeader(header: PageHeader) {
  header.title = htmlDecode(header.title)
  header.children.forEach(child => fixPageHeader(child))
}

export default defineUserConfig({
  lang: 'en-GB',
  title: 'voice-orders',
  description: 'Speak a phrase, press the keys: a Linux-native voice macro tool.',

  // This site is published to GitHub Pages as a project page (no custom
  // domain, and therefore no CNAME), so every asset lives under /voice-rs/.
  base: '/voice-rs/',

  head: [
    ['meta', { name: "description", content: "Documentation for voice-orders, a Linux-native voice macro tool which turns spoken phrases into keystrokes in your games." }],
  ],

  extendsPage(page, _app) {
    const fixedHeaders = page.headers || []
    fixedHeaders.forEach(header => fixPageHeader(header))
  },

  bundler: viteBundler(),

  theme: defaultTheme({
    logo: 'https://cdn.sierrasoftworks.com/logos/icon.png',
    logoDark: 'https://cdn.sierrasoftworks.com/logos/icon_light.png',

    repo: "SierraSoftworks/voice-rs",
    docsRepo: "SierraSoftworks/voice-rs",
    docsDir: 'docs',
    navbar: [
      {
        text: "Guide",
        link: "/guide/",
        children: [
          '/guide/README.md',
          '/guide/installation.md',
          '/guide/permissions.md',
          '/guide/steam.md',
        ]
      },
      {
        text: "Profiles",
        link: "/profiles/",
      },
      {
        text: "Grammar",
        link: "/grammar/",
      },
      {
        text: "Keys",
        link: "/keys/",
      },
      {
        text: "Download",
        link: "https://github.com/SierraSoftworks/voice-rs/releases",
        target: "_blank"
      },
      {
        text: "Report an Issue",
        link: "https://github.com/SierraSoftworks/voice-rs/issues/new",
        target: "_blank"
      }
    ],

    sidebar: {
      '/guide/': [
        {
          text: "Guide",
          children: [
            '/guide/README.md',
            '/guide/installation.md',
            '/guide/permissions.md',
            '/guide/steam.md',
          ]
        }
      ],
      '/profiles/': [
        {
          text: "Profiles",
          children: [
            '/profiles/README.md',
          ]
        }
      ],
      '/grammar/': [
        {
          text: "Grammar",
          children: [
            '/grammar/README.md',
          ]
        }
      ],
      '/keys/': [
        {
          text: "Keys",
          children: [
            '/keys/README.md',
          ]
        }
      ]
    }
  }),
})
