import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import HomepageFeatures from '@site/src/components/HomepageFeatures';
import Heading from '@theme/Heading';

import styles from './index.module.css';

function HomepageHeader() {
  const {siteConfig} = useDocusaurusContext();

  return (
    <header className={styles.heroBanner}>
      <div className={styles.heroShell}>
        <img className={styles.heroMark} src="/img/logo.svg" alt="" aria-hidden="true" />
        <p className={styles.heroEyebrow}>Distributed evaluation infrastructure</p>
        <Heading as="h1" className={styles.heroTitle}>
          {siteConfig.title}
        </Heading>
        <p className={styles.heroSubtitle}>{siteConfig.tagline}</p>
        <p className={styles.heroText}>
          Publish versioned WASM evaluators, run durable agent evaluations, and
          gate releases with structured results your automation can trust.
        </p>
        <div className={styles.buttons}>
          <Link className={styles.primaryButton} to="/docs/guides/getting-started">
            Get Started
          </Link>
          <Link className={styles.secondaryButton} to="/docs/architecture/flows/">
            View Flows
          </Link>
        </div>
        <div className={styles.signalBar} aria-label="Agent Vigilo capabilities">
          <span>WASM evaluators</span>
          <span>durable runs</span>
          <span>CI gates</span>
        </div>
      </div>
    </header>
  );
}

export default function Home(): ReactNode {
  const {siteConfig} = useDocusaurusContext();

  return (
    <Layout description={siteConfig.tagline}>
      <HomepageHeader />
      <main>
        <HomepageFeatures />
      </main>
    </Layout>
  );
}
