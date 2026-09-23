import {ComparisonSpec} from './ComparisonScene';

const RED = '#d97a6b';
const GREEN = '#3fb950';
const BLUE = '#58a6ff';

const topY = 150;
const setupY = 520;
const sessY = 780;

const topXs = [140, 360, 580, 800, 1020, 1240, 1460];
const setupXs = [600, 830, 1060, 1290];
const sessXs = [1440, 1200, 960, 720, 480, 240];

const setup = {
	nodes: [
		{x: setupXs[0], y: setupY, label: 'Repo', sub: 'any language', icon: 'repo' as const},
		{x: setupXs[1], y: setupY, label: 'prepare-repo', sub: 'index + graph', icon: 'init' as const},
		{x: setupXs[2], y: setupY, label: '.pixel/', sub: 'shards · call graph', icon: 'graph' as const},
		{x: setupXs[3], y: setupY, label: 'In Repo', sub: 'git-anchored', icon: 'git' as const},
	],
	badge: 'FRESH EVERY HEAD',
};

export const measuredSavingsSpec: ComparisonSpec = {
	width: 1600,
	height: 1000,
	accentA: RED,
	accentB: BLUE,
	dotColor: GREEN,
	panelA: {
		title: 'WITHOUT PIXEL',
		nodes: [
			{x: topXs[0], y: topY, label: 'Task', sub: 'new session', icon: 'task'},
			{x: topXs[1], y: topY, label: 'Agent', sub: 'zero memory', icon: 'agent'},
			{x: topXs[2], y: topY, label: 'Blind Reads', sub: 'rg · cat · sed', icon: 'search'},
			{x: topXs[3], y: topY, label: 'Dead Ends', sub: 'wrong files', icon: 'deadend', warn: true},
			{x: topXs[4], y: topY, label: 'Guess Impact', sub: 're-derive', icon: 'guess'},
			{x: topXs[5], y: topY, label: 'Task Shipped', sub: 'budget spent', icon: 'ship'},
			{x: topXs[6], y: topY, label: 'Savings', sub: 'claimed, unmeasured', icon: 'lost'},
		],
		loop: "REPEATS EVERY SESSION · EVERY TEAMMATE'S AGENT",
	},
	panelB: {
		title: 'WITH PIXEL',
		leftText: ['INDEX ALREADY', 'KNOWS YOUR', 'REPO'],
		setup,
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'Session Hook', sub: 'use pixel first', icon: 'hook', accent: GREEN},
				{x: sessXs[1], y: sessY, label: 'Task', sub: 'new session', icon: 'task'},
				{x: sessXs[2], y: sessY, label: 'Agent', sub: 'asks pixel', icon: 'agent'},
				{x: sessXs[3], y: sessY, label: 'find-code · impact', sub: 'bounded reads', icon: 'read', accent: GREEN},
				{x: sessXs[4], y: sessY, label: 'Precise Context', sub: 'no guessing', icon: 'context'},
				{x: sessXs[5], y: sessY, label: 'token-savings', sub: 'measured', icon: 'meter', accent: GREEN},
			],
		},
		loop: 'ONE .PIXEL DIR · EVERY AGENT · SAVINGS MEASURED, NOT CLAIMED',
	},
};

export const impactSpec: ComparisonSpec = {
	width: 1600,
	height: 1000,
	accentA: RED,
	accentB: BLUE,
	dotColor: GREEN,
	panelA: {
		title: 'WITHOUT PIXEL',
		nodes: [
			{x: topXs[0], y: topY, label: 'Task', sub: 'rename the fn', icon: 'task'},
			{x: topXs[1], y: topY, label: 'Agent', sub: 'edits blind', icon: 'edit'},
			{x: topXs[2], y: topY, label: 'Grep Callers', sub: 'partial hits', icon: 'search'},
			{x: topXs[3], y: topY, label: 'Missed Sites', sub: 'dynamic · aliased', icon: 'deadend', warn: true},
			{x: topXs[4], y: topY, label: 'Red CI', sub: 'broken callers', icon: 'break'},
			{x: topXs[5], y: topY, label: 'Rework', sub: 'round trips', icon: 'guess'},
			{x: topXs[6], y: topY, label: 'Trust', sub: 'eroded', icon: 'lost'},
		],
		loop: 'REPEATS EVERY REFACTOR · EVERY REPO',
	},
	panelB: {
		title: 'WITH PIXEL',
		leftText: ['CALL GRAPH', 'ALREADY', 'RESOLVED'],
		setup,
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'Session Hook', sub: 'impact before edit', icon: 'hook', accent: GREEN},
				{x: sessXs[1], y: sessY, label: 'Task', sub: 'rename the fn', icon: 'task'},
				{x: sessXs[2], y: sessY, label: 'pixel impact', sub: 'callers upstream', icon: 'graph', accent: GREEN},
				{x: sessXs[3], y: sessY, label: 'Blast Radius', sub: 'every site listed', icon: 'context'},
				{x: sessXs[4], y: sessY, label: 'Edit', sub: 'informed', icon: 'edit'},
				{x: sessXs[5], y: sessY, label: 'Green CI', sub: 'verified', icon: 'shield', accent: GREEN},
			],
		},
		loop: 'IMPACT BEFORE EDIT · EVERY CALLER COUNTED',
	},
};
