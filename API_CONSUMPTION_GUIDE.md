# Rinha de Backend 2026 - Leaderboard API Consumption Guide

This guide provides the exact API calls and code patterns needed to consume the rinha-de-backend-2026 GitHub repository and build a real-time leaderboard.

---

## QUICK START: Core API Calls

### 1. List All Submission Issues

**Endpoint:**
```
GET https://api.github.com/repos/zanfranceschi/rinha-de-backend-2026/issues?state=closed&per_page=100
```

**Headers:**
```
Authorization: token $GITHUB_TOKEN
Accept: application/vnd.github.v3+json
```

**Query Parameters:**
| Parameter | Value | Purpose |
|---|---|---|
| `state` | `closed` | Only completed benchmarks |
| `per_page` | `100` | Max results per page |
| `page` | `1, 2, 3...` | Pagination |

**Response Fields (relevant):**
```javascript
{
  number: 41,                    // Issue ID (unique identifier)
  title: "test nginx final",     // Submission identifier
  user: {
    login: "Lothyriel"           // Participant username
  },
  state: "closed",               // Status
  created_at: "2026-04-22T10:42:29Z",
  html_url: "https://github.com/...",
  comments: 1                    // Should be ≥1 for valid results
}
```

**Example curl:**
```bash
curl -H "Authorization: token $GITHUB_TOKEN" \
  "https://api.github.com/repos/zanfranceschi/rinha-de-backend-2026/issues?state=closed&per_page=100&page=1"
```

---

### 2. Fetch Comments for Specific Issue

**Endpoint:**
```
GET https://api.github.com/repos/zanfranceschi/rinha-de-backend-2026/issues/{issue_number}/comments
```

**Example (issue #41):**
```
https://api.github.com/repos/zanfranceschi/rinha-de-backend-2026/issues/41/comments
```

**Response Fields (relevant):**
```javascript
[
  {
    user: {
      login: "arinhadebackend"   // CI bot username (look for this!)
    },
    body: "test result:\n```json\n{...JSON...}\n```",
    created_at: "2026-04-22T10:47:32Z"
  },
  // ... other comments (usually only 1)
]
```

**JSON Extraction Pattern:**
```javascript
// Extract JSON from Markdown code block
const match = body.match(/```json\n([\s\S]*?)\n```/);
const jsonString = match ? match[1] : null;
const result = JSON.parse(jsonString);
```

---

### 3. Get Participant Registration Data

**File:** `/participants/{username}.json` in the repo  
**URL (via GitHub raw content):**
```
https://raw.githubusercontent.com/zanfranceschi/rinha-de-backend-2026/main/participants/Lothyriel.json
```

**Response:**
```json
[
  {
    "id": "lothyriel-rust",
    "repo": "https://github.com/lothyriel/rinha_2026"
  }
]
```

**If using gh CLI:**
```bash
gh api repos/zanfranceschi/rinha-de-backend-2026/contents/participants/Lothyriel.json \
  --jq '.content | @base64d | fromjson'
```

---

## Complete JavaScript Implementation

```javascript
const REPO = 'zanfranceschi/rinha-de-backend-2026';
const GITHUB_TOKEN = process.env.GITHUB_TOKEN;

// Helper to make API calls
async function githubApi(endpoint, options = {}) {
  const response = await fetch(
    `https://api.github.com${endpoint}`,
    {
      headers: {
        'Authorization': `token ${GITHUB_TOKEN}`,
        'Accept': 'application/vnd.github.v3+json',
        ...options.headers
      },
      ...options
    }
  );
  
  if (!response.ok) {
    throw new Error(`GitHub API error: ${response.status} ${response.statusText}`);
  }
  
  return response.json();
}

// Step 1: Fetch all closed issues
async function fetchAllSubmissions() {
  const submissions = [];
  let page = 1;
  let hasMore = true;
  
  while (hasMore) {
    const issues = await githubApi(
      `/repos/${REPO}/issues?state=closed&per_page=100&page=${page}`
    );
    
    // Filter out PRs and issues without comments
    const validIssues = issues.filter(
      issue => !issue.pull_request && issue.comments >= 1
    );
    
    submissions.push(...validIssues);
    hasMore = issues.length === 100; // If we got fewer than 100, we're done
    page++;
  }
  
  return submissions;
}

// Step 2: Group by participant, keep only latest
function getLatestPerParticipant(submissions) {
  const grouped = {};
  
  for (const issue of submissions) {
    const user = issue.user.login;
    
    if (!grouped[user] || issue.number > grouped[user].number) {
      grouped[user] = issue;
    }
  }
  
  return Object.values(grouped);
}

// Step 3: Extract JSON result from comment
async function fetchResultJson(issueNumber) {
  const comments = await githubApi(
    `/repos/${REPO}/issues/${issueNumber}/comments`
  );
  
  // Find the result comment (posted by the CI bot)
  const resultComment = comments.find(
    c => c.user.login === 'arinhadebackend' && c.body.includes('test result:')
  );
  
  if (!resultComment) {
    return null;
  }
  
  // Extract JSON from markdown code block
  const match = resultComment.body.match(/```json\n([\s\S]*?)\n```/);
  const jsonString = match ? match[1] : null;
  
  if (!jsonString) {
    return null;
  }
  
  return JSON.parse(jsonString);
}

// Step 4: Fetch participant registration
async function fetchParticipantInfo(username) {
  try {
    const response = await fetch(
      `https://raw.githubusercontent.com/${REPO}/main/participants/${username}.json`
    );
    return response.json();
  } catch {
    return null;
  }
}

// Step 5: Build leaderboard entry
async function buildLeaderboardEntry(issue) {
  const username = issue.user.login;
  const result = await fetchResultJson(issue.number);
  
  if (!result) {
    return null;
  }
  
  const participantInfo = await fetchParticipantInfo(username);
  const scoring = result['test-results']?.scoring || {};
  const responseTime = result['test-results']?.response_times || {};
  const breakdown = scoring.breakdown || {};
  
  return {
    // Identity
    participant_username: username,
    submission_id: participantInfo?.[0]?.id || username,
    submission_repo: participantInfo?.[0]?.repo || null,
    
    // Issue info
    issue_number: issue.number,
    issue_url: issue.html_url,
    issue_title: issue.title,
    created_at: issue.created_at,
    
    // Leaderboard metrics
    final_score: scoring.final_score || 0,
    raw_score: scoring.raw_score || 0,
    detection_accuracy: scoring.detection_accuracy || "0%",
    latency_multiplier: scoring.latency_multiplier || 1,
    
    // Performance metrics
    p99_ms: parseFloat(responseTime.p99?.replace('ms', '')) || null,
    p90_ms: parseFloat(responseTime.p90?.replace('ms', '')) || null,
    median_ms: parseFloat(responseTime.med?.replace('ms', '')) || null,
    
    // Detection breakdown
    true_positives: breakdown.true_positive_detections || 0,
    true_negatives: breakdown.true_negative_detections || 0,
    false_positives: breakdown.false_positive_detections || 0,
    false_negatives: breakdown.false_negative_detections || 0,
    http_errors: breakdown.http_errors || 0,
    
    // Runtime info
    cpu_count: result['runtime-info']?.cpu || 1,
    memory_mb: result['runtime-info']?.mem || 0,
    commit: result['runtime-info']?.commit || null
  };
}

// Main: Build complete leaderboard
async function buildLeaderboard() {
  console.log('Fetching submissions...');
  const allSubmissions = await fetchAllSubmissions();
  
  console.log(`Found ${allSubmissions.length} total submissions`);
  
  const latest = getLatestPerParticipant(allSubmissions);
  console.log(`Processing ${latest.length} latest per participant...`);
  
  const leaderboard = [];
  
  for (const issue of latest) {
    try {
      console.log(`  → Processing issue #${issue.number} from ${issue.user.login}...`);
      const entry = await buildLeaderboardEntry(issue);
      if (entry) {
        leaderboard.push(entry);
      }
    } catch (error) {
      console.error(`    Error: ${error.message}`);
    }
  }
  
  // Sort by final_score (descending), then by created_at (most recent first)
  leaderboard.sort((a, b) => {
    if (b.final_score !== a.final_score) {
      return b.final_score - a.final_score;
    }
    return new Date(b.created_at) - new Date(a.created_at);
  });
  
  return leaderboard;
}

// Export for use
module.exports = { buildLeaderboard };

// If run directly
if (require.main === module) {
  buildLeaderboard()
    .then(leaderboard => {
      console.log('\n=== LEADERBOARD ===');
      console.table(
        leaderboard.map(entry => ({
          Rank: leaderboard.indexOf(entry) + 1,
          Username: entry.participant_username,
          Score: entry.final_score,
          Accuracy: entry.detection_accuracy,
          'P99 (ms)': entry.p99_ms
        }))
      );
    })
    .catch(console.error);
}
```

---

## Rate Limiting Strategy

### Calculate Your Quota
```
Per submission: 1 API call for comments + 1 API call for participant info = 2 calls
For 10 participants: ~20 calls
For 50 participants: ~100 calls
```

### Recommended Caching
```javascript
// Cache results for 5 minutes to reduce API calls on repeated builds
const cache = new Map();
const CACHE_TTL_MS = 5 * 60 * 1000;

function getCached(key) {
  const cached = cache.get(key);
  if (cached && Date.now() - cached.timestamp < CACHE_TTL_MS) {
    return cached.value;
  }
  cache.delete(key);
  return null;
}

function setCached(key, value) {
  cache.set(key, { value, timestamp: Date.now() });
}
```

### Monitor Rate Limits
```javascript
async function checkRateLimit() {
  const response = await fetch('https://api.github.com/rate_limit', {
    headers: { 'Authorization': `token $GITHUB_TOKEN}` }
  });
  const data = await response.json();
  const remaining = data.resources.core.remaining;
  const reset = new Date(data.resources.core.reset * 1000);
  
  console.log(`Rate limit: ${remaining} remaining, resets at ${reset}`);
  
  if (remaining < 100) {
    throw new Error('Rate limit almost exhausted');
  }
}
```

---

## Error Handling Patterns

### Handle Missing Data
```javascript
function safeExtractNumber(str) {
  if (!str) return null;
  const match = str.match(/[\d.]+/);
  return match ? parseFloat(match[0]) : null;
}

function safeExtractPercent(str) {
  if (!str) return "0%";
  // Already in format "99.55%" - return as-is
  return str;
}
```

### Validate JSON Structure
```javascript
function validateResult(result) {
  const required = ['runtime-info', 'test-results'];
  for (const key of required) {
    if (!result[key]) {
      throw new Error(`Missing required field: ${key}`);
    }
  }
  
  const scoring = result['test-results']?.scoring;
  if (!scoring?.final_score) {
    throw new Error('Missing final_score');
  }
  
  return true;
}
```

---

## Testing Your Implementation

### Test with Specific Issue
```bash
# Manually fetch issue 41's result
gh api repos/zanfranceschi/rinha-de-backend-2026/issues/41/comments \
  --jq '.[0].body'
```

### Verify Participant Mapping
```bash
# Check all participants
for user in Lothyriel MXLange passos-matheus viniciusdsandrade zanfranceschi; do
  echo "=== $user ==="
  gh api repos/zanfranceschi/rinha-de-backend-2026/contents/participants/$user.json \
    --jq '.content | @base64d | fromjson'
done
```

### Validate Rate Limits
```bash
gh api rate_limit --jq '.resources | {core: .core.remaining, reset: .core.reset}'
```

---

## Monitoring & Refresh Strategy

### Recommended Refresh Frequency
- **Every 5 minutes:** Fetch latest issues and cache
- **Every 30 minutes:** Force refresh participant data
- **On-demand:** When user clicks "Refresh" button

### Batch Processing
```javascript
async function updateLeaderboard() {
  try {
    // Fetch fresh data
    const leaderboard = await buildLeaderboard();
    
    // Store in database/cache
    await db.updateLeaderboard(leaderboard);
    
    // Update last sync time
    await db.setLastSync(new Date());
    
    return leaderboard;
  } catch (error) {
    console.error('Failed to update leaderboard:', error);
    // Return cached data if available
    return await db.getLastLeaderboard();
  }
}
```

---

## Environment Variables

Create a `.env` file:
```
GITHUB_TOKEN=ghp_xxxxxxxxxxxxxxxxxxxx
GITHUB_REPO=zanfranceschi/rinha-de-backend-2026
REFRESH_INTERVAL_MS=300000
CACHE_TTL_MS=300000
```

Load in your app:
```javascript
require('dotenv').config();

const GITHUB_TOKEN = process.env.GITHUB_TOKEN;
const REPO = process.env.GITHUB_REPO;
const REFRESH_INTERVAL = parseInt(process.env.REFRESH_INTERVAL_MS || '300000');
```

---

## Troubleshooting

| Issue | Solution |
|-------|----------|
| 401 Unauthorized | Check `GITHUB_TOKEN` is valid and has `public_repo` scope |
| 403 Rate Limited | Wait until reset time or use cache |
| 404 Not Found | Verify issue number exists and is in `state=closed` |
| Empty JSON | Check comment body format matches ` ```json\n...\n``` ` |
| Parse Error | Validate JSON with `JSON.parse()` in try-catch |
| Null participant | Some users may not be registered in `/participants/` |

