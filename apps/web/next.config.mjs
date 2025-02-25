/** @type {import('next').NextConfig} */

import("dotenv").then(({ config }) => config({ path: "../../.env" }));

if (process.env.DB_PLANETSCALE_HOST !== undefined) {
  throw new Error(
    "DB_PLANETSCALE_HOST no longer supported. Use DATABASE_URL instead. Must start with mysql:// for local dev, and https:// for production."
  );
}

import fs from "fs";
import path from "path";

const packageJson = JSON.parse(
  fs.readFileSync(path.resolve("./package.json"), "utf8")
);
const { version } = packageJson;

const nextConfig = {
  reactStrictMode: true,
  swcMinify: true,
  transpilePackages: ["@cap/ui", "@cap/utils"],
  eslint: {
    ignoreDuringBuilds: true,
  },
  typescript: {
    ignoreBuildErrors: true,
  },
  experimental: {
    optimizePackageImports: ["@cap/ui", "@cap/utils"],
    serverComponentsExternalPackages: [
      "@react-email/components",
      "@react-email/render",
      "@react-email/tailwind",
    ],
  },
  images: {
    remotePatterns: [
      {
        protocol: "https",
        hostname: "**",
        port: "",
        pathname: "**",
      },
    ],
  },
  async rewrites() {
    return [
      {
        source: "/r/:path*",
        destination: "https://dub.cap.link/:path*",
      },
    ];
  },
  async redirects() {
    return [
      {
        source: "/roadmap",
        destination:
          "https://capso.notion.site/7aac740edeee49b5a23be901a7cb734e?v=9d4a3bf3d72d488cad9b899ab73116a1",
        permanent: true,
      },
    ];
  },
  env: {
    appVersion: version,
  },
};

export default nextConfig;
