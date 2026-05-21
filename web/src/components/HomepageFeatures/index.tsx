import type {ReactNode} from 'react';
import Heading from '@theme/Heading';
import styles from './styles.module.css';

type FeatureItem = {
  label: string;
  title: string;
  description: ReactNode;
};

const FeatureList: FeatureItem[] = [
  {
    label: '01',
    title: 'Publish evaluator artifacts',
    description: (
      <>
        Version WASM evaluators once, reference them from profiles, and keep
        scoring logic stable across local runs, CI, and production gates.
      </>
    ),
  },
  {
    label: '02',
    title: 'Run distributed evaluations',
    description: (
      <>
        Coordinators dispatch durable run chunks while workers call the target
        agent, execute evaluators, and persist normalized results.
      </>
    ),
  },
  {
    label: '03',
    title: 'Gate releases with evidence',
    description: (
      <>
        Watch pass/fail outcomes, inspect summaries, and export execution
        evidence for release decisions and debugging.
      </>
    ),
  },
];

function Feature({label, title, description}: FeatureItem) {
  return (
    <article className={styles.feature}>
      <span className={styles.featureLabel}>{label}</span>
      <Heading as="h3" className={styles.featureTitle}>
        {title}
      </Heading>
      <p className={styles.featureText}>{description}</p>
    </article>
  );
}

export default function HomepageFeatures(): ReactNode {
  return (
    <section className={styles.features}>
      <div className={styles.inner}>
        <div className={styles.sectionHeader}>
          <span>What you get</span>
          <Heading as="h2">Evaluation infrastructure, not another score script.</Heading>
        </div>
        <div className={styles.featureGrid}>
          {FeatureList.map((props) => (
            <Feature key={props.label} {...props} />
          ))}
        </div>
      </div>
    </section>
  );
}
