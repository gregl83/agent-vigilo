import type {ReactNode} from 'react';
import {useEffect} from 'react';
import Link from '@docusaurus/Link';
import Head from '@docusaurus/Head';
import Layout from '@theme/Layout';

const TARGET = '/docs/guides/';

export default function GuidesCategoryRedirect(): ReactNode {
  useEffect(() => {
    window.location.replace(TARGET);
  }, []);

  return (
    <Layout
      title="Guides"
      description="Agent Vigilo guides for getting started, creating evaluators, and publishing evaluator artifacts.">
      <Head>
        <link rel="canonical" href="https://agentvigilo.com/docs/guides/" />
        <meta name="robots" content="noindex,follow" />
        <meta httpEquiv="refresh" content={`0; url=${TARGET}`} />
      </Head>
      <main className="container margin-vert--xl">
        <div className="row">
          <div className="col col--8 col--offset-2">
            <h1>Guides</h1>
            <p>This indexed category URL has moved to the maintained guides page.</p>
            <Link className="button button--primary" to={TARGET}>
              Open Guides
            </Link>
          </div>
        </div>
      </main>
    </Layout>
  );
}
