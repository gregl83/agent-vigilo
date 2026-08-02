import Link from '@docusaurus/Link';
import * as Popover from '@radix-ui/react-popover';
import React, {type ReactNode} from 'react';

import {
  glossaryEntries,
  glossarySectionById,
  type GlossaryEntry,
} from '../../data/glossary';
import styles from './styles.module.css';

type GlossaryTermProps = {
  children?: ReactNode;
  term: string;
};

type GlossarySectionProps = {
  section: string;
};

function richText(value: string): ReactNode {
  return value.split(/(`[^`]+`)/g).map((part, index) =>
    part.startsWith('`') && part.endsWith('`') ? (
      <code key={`${part}-${index}`}>{part.slice(1, -1)}</code>
    ) : (
      part
    ),
  );
}

function requireEntry(id: string): GlossaryEntry {
  const entry = glossaryEntries.get(id);
  if (!entry) {
    throw new Error(`Unknown glossary term: ${id}`);
  }
  return entry;
}

export function GlossaryTerm({children, term}: GlossaryTermProps): ReactNode {
  const entry = requireEntry(term);

  return (
    <Popover.Root>
      <Popover.Trigger asChild>
        <button type="button" className={styles.trigger}>
          {children ?? entry.term}
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          className={styles.content}
          sideOffset={8}
          collisionPadding={16}>
          <div className={styles.header}>
            <strong className={styles.title}>{entry.term}</strong>
            <Popover.Close className={styles.close} aria-label="Close definition">
              &times;
            </Popover.Close>
          </div>
          <p className={styles.definition}>{richText(entry.definition)}</p>
          <p className={styles.scope}>{richText(entry.scope)}</p>
          <Link className={styles.link} to={`/docs/glossary#${entry.id}`}>
            View in glossary
          </Link>
          <Popover.Arrow className={styles.arrow} />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

export function GlossarySection({section}: GlossarySectionProps): ReactNode {
  const value = glossarySectionById.get(section);
  if (!value) {
    throw new Error(`Unknown glossary section: ${section}`);
  }

  return (
    <dl className={styles.glossaryList}>
      {value.entries.map((entry) => (
        <div className={styles.glossaryRow} key={entry.id}>
          <dt className={styles.glossaryTerm} id={entry.id}>
            {entry.term}
          </dt>
          <dd className={styles.glossaryDefinition}>{richText(entry.definition)}</dd>
          <dd className={styles.glossaryScope}>{richText(entry.scope)}</dd>
        </div>
      ))}
    </dl>
  );
}
