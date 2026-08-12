import {readdir, readFile} from 'node:fs/promises';
import path from 'node:path';

const BUILD_DIRECTORY = path.resolve('build');
const SITE_ORIGIN = 'https://agentvigilo.com';

async function findIndexFiles(directory) {
  const entries = await readdir(directory, {withFileTypes: true});
  const files = await Promise.all(
    entries.map(async (entry) => {
      const entryPath = path.join(directory, entry.name);
      return entry.isDirectory() ? findIndexFiles(entryPath) : entryPath.endsWith(`${path.sep}index.html`) ? [entryPath] : [];
    }),
  );
  return files.flat();
}

function readAttribute(tag, name) {
  const match = tag.match(new RegExp(`\\s${name}=(?:"([^"]*)"|'([^']*)'|([^\\s>]+))`, 'i'));
  return match?.[1] ?? match?.[2] ?? match?.[3];
}

function findTag(html, element, attribute, value) {
  return html
    .match(new RegExp(`<${element}\\b[^>]*>`, 'gi'))
    ?.find((tag) => readAttribute(tag, attribute)?.toLowerCase() === value);
}

function normalizeUrl(value) {
  const url = new URL(value, SITE_ORIGIN);
  url.pathname = url.pathname === '/' ? '/' : url.pathname.replace(/\/$/, '');
  return url.href;
}

const sitemap = await readFile(path.join(BUILD_DIRECTORY, 'sitemap.xml'), 'utf8');
const sitemapUrls = [...sitemap.matchAll(/<loc>([^<]+)<\/loc>/g)].map((match) => normalizeUrl(match[1]));
const sitemapSet = new Set(sitemapUrls);
const errors = [];

if (sitemapSet.size !== sitemapUrls.length) {
  errors.push('sitemap.xml contains duplicate URLs');
}

const indexFiles = await findIndexFiles(BUILD_DIRECTORY);
const generatedCanonicalUrls = new Set();

for (const indexFile of indexFiles) {
  const html = await readFile(indexFile, 'utf8');
  const canonicalTag = findTag(html, 'link', 'rel', 'canonical');
  const robotsTag = findTag(html, 'meta', 'name', 'robots') ?? findTag(html, 'meta', 'property', 'robots');
  const robots = robotsTag ? readAttribute(robotsTag, 'content')?.toLowerCase() : undefined;
  const canonical = canonicalTag ? readAttribute(canonicalTag, 'href') : undefined;
  const route = `/${path.relative(BUILD_DIRECTORY, path.dirname(indexFile)).split(path.sep).join('/')}`.replace(/\/$/, '') || '/';

  if (!canonical) {
    errors.push(`${route} has no canonical URL`);
    continue;
  }

  const canonicalUrl = normalizeUrl(canonical);
  generatedCanonicalUrls.add(canonicalUrl);

  if (robots?.split(',').map((value) => value.trim()).includes('noindex')) {
    if (sitemapSet.has(canonicalUrl) && normalizeUrl(`${SITE_ORIGIN}${route}`) === canonicalUrl) {
      errors.push(`${route} is noindex but appears in sitemap.xml`);
    }
  } else if (!sitemapSet.has(canonicalUrl)) {
    errors.push(`${route} is indexable but its canonical URL is missing from sitemap.xml`);
  }
}

for (const sitemapUrl of sitemapSet) {
  if (!generatedCanonicalUrls.has(sitemapUrl)) {
    errors.push(`${sitemapUrl} has no generated canonical page`);
  }
}

if (errors.length > 0) {
  console.error(`Sitemap validation failed:\n- ${errors.join('\n- ')}`);
  process.exitCode = 1;
} else {
  console.log(`Sitemap validation passed: ${sitemapSet.size} canonical pages.`);
}
