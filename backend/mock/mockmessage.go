package mock

import (
	"bytes"
	_ "embed"
	"html/template"
	"math/rand"
	"os/user"
	"time"

	"github.com/roblillack/elma/docs"
	"github.com/roblillack/elma/models"
)

func randomString(options ...string) string {
	return options[rand.Intn(len(options))]
}

var emailSubjects = []string{
	"Project Update: Q4 Progress and Next Steps",
	"Meeting Agenda for Client Presentation on Nov 15",
	"Deadline Reminder: Submit Reports by EOD Friday",
	"Team Offsite Planning: RSVP Required",
	"Follow-Up: Action Items from Yesterday’s Call",
	"Monthly Performance Review – Feedback Needed",
	"Budget Allocation Update: Important Adjustments",
	"Invitation: Quarterly Strategy Meeting",
	"Client Feedback on Latest Release",
	"Employee Benefits Review – Important Changes",
	"Please Update Your Contact Information",
	"IT Security Update – Action Required",
	"Weekly Team Meeting Agenda",
	"Sales Forecast for Next Quarter",
	"Vacation Schedule – Submit Requests",
	"Office Closed on Monday for Holiday",
	"Performance Bonus Criteria – Important Update",
	"New Software Rollout: Training Session",
	"Expense Report Submission Reminder",
	"Presentation Feedback – Great Job!",
	"Client Contract Renewal Update",
	"Upcoming Deadline: Submit Draft by Friday",
	"Office Potluck – Sign Up to Bring a Dish!",
	"Company Newsletter – October Edition",
	"Invitation: Leadership Development Workshop",
	"Review Your Training Completion Status",
	"Team Member Recognition – This Month’s Star!",
	"Updated Code of Conduct – Please Review",
	"Payroll Update: Changes to Direct Deposit",
	"New Team Member Introduction",
	"Safety Protocol Reminder – Please Review",
	"Invitation: Customer Success Webinar",
	"Update: Office Safety and Hygiene Guidelines",
	"Congratulations on Your Work Anniversary!",
	"Feedback Requested on Project XYZ",
	"Data Privacy Policy Update",
	"Invitation: Diversity and Inclusion Workshop",
	"Resource Allocation Changes for Team Leads",
	"Open Enrollment for Health Benefits",
	"All Hands Meeting This Thursday – Don’t Miss!",
	"Customer Service Recognition Program",
	"Team Training: New Skill Building Opportunities",
	"End of Year Goals – Self Assessment Reminder",
	"New Product Launch: Marketing Material Access",
	"Mandatory Compliance Training Reminder",
	"Sales Target Update for November",
	"Updated Policy on Remote Work Days",
	"Thank You for Going Above and Beyond!",
	"Request for Performance Feedback – Q3",
	"Training: Advanced Excel Skills",
	"Welcome Our New Interns!",
	"Department Budget Review: Meeting Agenda",
	"Productivity Tips and Tools from HR",
	"Reminder: Submit Your Timesheet",
	"Software Maintenance – Expected Downtime",
	"Security Alert: Phishing Scams Detected",
	"Mentorship Program – Apply Today!",
	"Expense Policy Update – Please Review",
	"Workshop Invitation: Effective Communication",
	"Marketing Campaign Feedback Needed",
	"Important: Update on Travel Reimbursement",
	"Data Security Best Practices",
	"Thank You for Your Dedication!",
	"Reminder: Submit Your Tax Information",
	"Annual Conference RSVP Confirmation",
	"Training Session on Effective Leadership",
	"Employee Wellness Program Details",
	"Q1 Objectives: Share Your Goals",
	"New Policy on Office Attendance",
	"Top 10 Tips for Maximizing Productivity",
	"Special Announcement: Promotion Round",
	"Client Project Feedback Survey",
	"Performance Goal Setting for Next Year",
	"Onboarding Checklist for New Joiners",
	"Updated Dress Code Policy",
	"Upcoming Webinar on Customer Engagement",
	"Recognition: Employee of the Month",
	"Survey: Employee Satisfaction Feedback",
	"Updated Holiday Schedule",
	"HR Portal – Update Your Profile Information",
	"Policy on Use of Personal Devices",
	"Staff Appreciation Day – Join the Celebration!",
	"Social Media Policy Guidelines",
	"Congratulations on a Great Quarter!",
	"IT Support Survey: Tell Us How We're Doing",
	"New Sustainability Initiatives Launched",
	"Reminder: Complete Your Performance Review",
	"Thank You for a Great Presentation!",
	"Employee Handbook Updates",
	"Key Announcements from Last Week's Meeting",
	"Tips for Managing Stress at Work",
	"Employee Recognition Program Update",
	"Invitation to Participate in Leadership Survey",
	"Congratulations on Meeting Your Targets!",
	"Weekly Task Review and Updates",
	"Important Changes to Working Hours",
	"Team Building Event Scheduled for Next Week",
	"Annual Leave Policy Update",
	"New Communication Tool Launched",
	"Health & Safety Training: Register Now",
	"Training Program Completion Certificate",
	"New Project Assignment: Team Leads Notified",
	"Reminder: Quarterly Tax Submission",
	"Invitation to Submit Team Highlights",
	"Congratulations on the Successful Project Launch!",
	"Important Reminder: Data Privacy Policy",
	"Appreciation: Thanks for Your Hard Work!",
	"Invitation to Submit Ideas for Innovation",
	"Key Takeaways from Recent Training Session",
	"Emergency Preparedness Guidelines",
	"Mandatory IT Security Training",
	"Team Appreciation Lunch – RSVP Now!",
	"Survey: What Are Your Learning Goals?",
	"New Project Timeline and Deadlines",
	"Guidelines for Expense Reporting",
	"Customer Satisfaction Report – Latest Update",
	"Employee Referral Program: Earn Rewards!",
	"Client Feedback – Excellent Work!",
	"Introduction to New Project Management Tools",
	"Thank You for Being a Team Player!",
	"Survey Results: Employee Engagement",
	"Policy Update on Sick Leave",
	"Upcoming Workshop on Emotional Intelligence",
	"Reminder: Submit Your Professional Goals",
	"Annual Budget Meeting Agenda",
	"Important: Update Your Emergency Contacts",
	"Invitation to Join Our Book Club",
	"Q4 Marketing Strategy Meeting",
	"Training Opportunity: Advanced Software Skills",
	"Top 5 Resources for Project Management",
	"Departmental Goal Setting Session",
	"Company Update: New Initiatives and Goals",
	"Workshop Invitation: Stress Management",
	"Important Deadlines for December",
	"Thank You for a Successful Quarter!",
	"Webinar: Customer Retention Strategies",
	"Key Dates for Project Milestones",
	"New Employee Handbook Now Available",
	"Recognition: Great Work on XYZ Project!",
	"Policy Update: Code of Ethics",
	"Team Survey: Tools and Resources Needed",
	"Upcoming Deadlines: Project XYZ",
	"Invitation: Monthly HR Q&A Session",
	"Thank You for Your Feedback!",
	"New Policies on Flexible Work Hours",
	"Congratulations on Exceeding Targets!",
	"Invitation to Participate in Employee Poll",
	"New Resources for Learning & Development",
	"End of Year Feedback: Your Thoughts Matter!",
	"Training: Building Positive Work Relationships",
	"Annual Report – Submission Deadline",
	"Project Update: Client Feedback Received",
	"Safety Protocols for Office Re-entry",
	"Updated Procedures for Business Travel",
	"Weekly Round-Up: Key Updates and News",
	"Special Acknowledgment for Team Leads",
	"Feedback Request on New Workflows",
	"Introduction to New Team Member",
	"Expense Reporting Guidelines",
	"Thank You for Your Hard Work!",
	"Tips for Staying Productive Remotely",
	"Important: System Downtime Notice",
	"Employee Poll: Remote Work Preferences",
	"Invitation: Monthly Team Lunch",
	"Annual Financial Summary and Updates",
	"Team Recognition for Recent Project Success",
	"Health and Wellness Tips",
	"New Health Benefits Options Available",
	"Reminder: Compliance Training",
	"Customer Satisfaction Survey Results",
	"Invitation: Annual Company Picnic",
	"Weekly Project Stand-Up Agenda",
	"Policy Reminder: Code of Conduct",
	"Upcoming Deadlines and Reminders",
	"Team Training: Skills for Success",
	"Special Announcement: Award Nominees",
	"Performance Feedback Request",
	"Remote Work Policy Update",
	"Time Management Workshop Invitation",
	"Invitation: Skills Development Webinar",
	"Recognition: Great Work from the XYZ Team",
	"Reminder: Expense Report Submission",
	"Guidelines for New Communication Tool",
	"Leadership Seminar Series: RSVP Now",
	"Thank You for Attending the Training!",
	"Survey: Employee Satisfaction Feedback",
	"Congratulations on Achieving Your Goals!",
	"Meeting Recap: Key Takeaways",
	"New Employee Benefits Options Available",
	"Invitation: Skill Development Sessions",
	"Guidelines for Annual Leave",
	"Project Deadline Reminder",
	"Office Updates: Health & Safety Measures",
	"Feedback Request: How Are We Doing?",
	"Invitation to Join Our Team Lunch",
	"New Tools and Resources Available",
	"Reminder: Performance Review Due Soon",
	"Customer Feedback: Excellent Work!",
	"Congratulations on Your Promotion!",
	"Updated Holiday Schedule for Next Year",
	"Employee Assistance Program Details",
	"Key Announcements from Leadership Team",
	"Training Session Recap and Feedback",
	"Invitation: Annual Recognition Ceremony",
	"Invitation: Employee Wellness Session",
	"Feedback Request: Recent Training Session",
	"End of Quarter Sales Targets",
	"Department Recognition for Q1 Performance",
	"Policy Reminder: Code of Conduct",
	"Congratulations on Completing Project XYZ!",
	"Thank You for Your Dedication!",
	"New Job Opening – Refer a Friend!",
	"Invitation: Customer Service Workshop",
	"Performance Goal Setting – Deadline",
	"Updated Vacation Policy",
	"Thank You for a Great Presentation!",
	"Employee Recognition Nomination",
	"IT Security Alert – Action Needed",
	"Employee Handbook Updates – Review Required",
	"Important Reminder: Annual Training",
	"Safety Protocols for Office Reopening",
	"Leadership Workshop: Sign Up Now!",
	"Project Kickoff Meeting Agenda",
	"New Training Opportunities Available",
	"Reminder: Complete Your Timesheet",
	"Team Social Event – RSVP Now!",
	"Important Update: Health & Safety Guidelines",
	"Customer Feedback Highlights",
	"Congratulations on Exceeding Your Goals!",
	"Tips for Managing Remote Work",
	"Recognition: Team XYZ Achievements",
	"Feedback Request: Training Session",
	"New Employee Referral Program",
	"Company-Wide Survey: We Want Your Input",
	"Upcoming Webinar on Career Development",
	"Congratulations on a Great Quarter!",
	"New Resources for Professional Development",
	"Thank You for Your Contribution!",
	"Policy Reminder: Data Security",
	"Invitation: Leadership Q&A Session",
	"End of Quarter Financial Summary",
	"Reminder: Complete Your Self-Assessment",
	"Feedback Request: Project XYZ",
	"Training Opportunity: Leadership Skills",
	"Employee Recognition: Monthly Award",
	"New Travel Reimbursement Guidelines",
	"Tips for Staying Organized",
	"Reminder: Expense Report Submission",
	"Thank You for Your Feedback!",
	"IT Maintenance Notice – Scheduled Downtime",
	"Congratulations on Reaching Your Milestone!",
	"Employee Satisfaction Survey Results",
	"Employee Assistance Program Details",
	"End of Year Deadlines and Reminders",
	"Team Recognition for Outstanding Work",
	"Invitation to Monthly Team Meeting",
	"Updated Remote Work Guidelines",
	"Congratulations on Completing XYZ Training!",
	"New Employee Handbook – Please Review",
	"Survey: Team Collaboration Tools",
	"New Employee Wellness Program",
	"Training Session: Advanced Excel",
	"Feedback Request: How Are We Doing?",
	"Congratulations on Your Recent Success!",
	"Reminder: Submit Your Timesheet",
	"Weekly Team Update",
	"Feedback Requested: Recent Training",
	"IT Alert: Phishing Scam Detected",
	"Updated Policy: Remote Work",
	"Survey: Employee Engagement",
}

var firstNames = []string{
	// Common Male Names
	"James", "John", "Robert", "Michael", "William",
	"David", "Richard", "Joseph", "Thomas", "Charles",
	"Christopher", "Daniel", "Matthew", "Anthony", "Mark",
	"Donald", "Steven", "Paul", "Andrew", "Joshua",
	"Kenneth", "Kevin", "Brian", "George", "Edward",
	"Ronald", "Timothy", "Jason", "Jeffrey", "Ryan",
	"Jacob", "Gary", "Nicholas", "Eric", "Stephen",
	"Jonathan", "Larry", "Justin", "Scott", "Brandon",
	"Benjamin", "Samuel", "Gregory", "Frank", "Alexander",
	"Raymond", "Patrick", "Jack", "Dennis", "Jerry",
	"Tyler", "Aaron", "Jose", "Adam", "Henry",
	"Nathan", "Douglas", "Zachary", "Peter", "Kyle",
	"Walter", "Ethan", "Jeremy", "Harold", "Keith",
	"Christian", "Roger", "Noah", "Gerald", "Carl",
	"Terry", "Sean", "Austin", "Arthur", "Lawrence",
	"Jesse", "Dylan", "Bryan", "Joe", "Jordan",
	"Billy", "Bruce", "Albert", "Willie", "Gabriel",
	"Logan", "Alan", "Juan", "Wayne", "Ralph",
	"Roy", "Eugene", "Randy", "Vincent", "Russell",
	"Louis", "Philip", "Bobby", "Johnny", "Bradley",

	// Common Female Names
	"Mary", "Patricia", "Jennifer", "Linda", "Elizabeth",
	"Barbara", "Susan", "Jessica", "Sarah", "Karen",
	"Nancy", "Lisa", "Margaret", "Betty", "Sandra",
	"Ashley", "Dorothy", "Kimberly", "Emily", "Donna",
	"Michelle", "Carol", "Amanda", "Melissa", "Deborah",
	"Stephanie", "Rebecca", "Sharon", "Laura", "Cynthia",
	"Kathleen", "Amy", "Shirley", "Angela", "Helen",
	"Anna", "Brenda", "Pamela", "Nicole", "Emma",
	"Samantha", "Katherine", "Christine", "Debra", "Rachel",
	"Catherine", "Carolyn", "Janet", "Ruth", "Maria",
	"Heather", "Diane", "Virginia", "Julie", "Joyce",
	"Victoria", "Olivia", "Kelly", "Christina", "Lauren",
	"Joan", "Evelyn", "Judith", "Megan", "Cheryl",
	"Andrea", "Hannah", "Martha", "Jacqueline", "Frances",
	"Gloria", "Ann", "Teresa", "Kathryn", "Sara",
	"Janice", "Jean", "Alice", "Madison", "Doris",
	"Abigail", "Julia", "Judy", "Grace", "Denise",
	"Amber", "Marilyn", "Beverly", "Danielle", "Theresa",
	"Sophia", "Marie", "Diana", "Brittany", "Natalie",
	"Isabella", "Charlotte", "Rose", "Alexis", "Kayla",

	// Unisex Names
	"Taylor", "Jordan", "Alex", "Morgan", "Casey",
	"Riley", "Avery", "Jamie", "Peyton", "Cameron",
	"Reese", "Dakota", "Skyler", "Emerson", "Rowan",
	"Quinn", "Harper", "Hayden", "Elliott", "Finley",
	"Drew", "Sawyer", "Jesse", "Phoenix", "Remy",
	"Mason", "Logan", "Charlie", "Jaden", "Spencer",
	"Parker", "Shawn", "Blake", "Sam", "Angel",
	"Terry", "Brett", "Reagan", "Aidan", "Alexis",
	"Eden", "River", "Dakota", "Dallas", "Micah",
	"Robin", "Carter", "Sidney", "Corey", "Shannon",
	"Kendall", "Rowan", "Elliot", "Casey", "Devin",
	"Jordan", "Sage", "Taylor", "Jayden", "Skylar",

	// Additional Names (Male)
	"Oscar", "Diego", "Marcus", "Maxwell", "Jorge",
	"Leonard", "Erik", "Miguel", "Carlos", "Edwin",
	"Landon", "Clayton", "Ruben", "Mario", "Travis",
	"Lorenzo", "Hector", "Eduardo", "Marvin", "Derek",
	"Armando", "Julian", "Pedro", "Marshall", "Dominic",
	"Adrian", "Colin", "Zane", "Bryce", "Dustin",
	"Jared", "Caleb", "Spencer", "Ivan", "Lance",
	"Toby", "Fernando", "Grayson", "Wade", "Wesley",
	"Simon", "Gavin", "Emmanuel", "Malcolm", "Andre",
	"Chase", "Javier", "Bennett", "Eli", "Mitchell",
	"Preston", "Oliver", "Bryant", "Graham", "Ezekiel",
	"Roberto", "Cristian", "Wyatt", "Sergio", "Tanner",

	// Additional Names (Female)
	"Faith", "Lucy", "Alyssa", "Lillian", "Ariana",
	"Jocelyn", "Valeria", "Leah", "Hailey", "Sophie",
	"Claire", "Makayla", "Trinity", "Molly", "Audrey",
	"Kylie", "Brooklyn", "Lilly", "Paige", "Eleanor",
	"Addison", "Savannah", "Aubrey", "Willow", "Arianna",
	"Clara", "Ivy", "Luna", "Melanie", "Vivian",
	"Alyssa", "Alexa", "Genesis", "Valentina", "Elena",
	"Hazel", "Esther", "Daisy", "Sadie", "Mckenzie",
	"Sienna", "Norah", "Delilah", "Eliza", "Mariana",
	"Allison", "Violet", "Brianna", "Nicole", "Melody",
	"Amelia", "Lucia", "Caroline", "Kennedy", "Serenity",
	"Georgia", "Laila", "Madeline", "Noelle", "Rebecca",
	"Rylee", "Maya", "Jade", "Bianca", "Juliana",
	"Raegan", "Chelsea", "Cecilia", "Daphne", "Camille",

	// More Modern/Popular Names
	"Liam", "Noah", "Oliver", "Elijah", "Lucas",
	"Mia", "Ava", "Sophia", "Isabella", "Amelia",
	"Ethan", "Aria", "Luna", "Mila", "Ella",
	"Jameson", "Sebastian", "Ezra", "Aiden", "Mason",
	"Hudson", "Levi", "Easton", "Hunter", "Nora",
	"Nova", "Scarlett", "Lily", "Aurora", "Riley",
	"Grayson", "Asher", "Xavier", "Zoe", "Isaiah",
	"Emmett", "Finn", "Everett", "Ryder", "Axel",
	"Nash", "Bennett", "Brooks", "Ryker", "Knox",
	"Bentley", "Beau", "Rowen", "Cash", "Maddox",
	"Maverick", "Paxton", "Walker", "Porter", "Haven",
	"Eliana", "Presley", "Oakley", "Harlow", "Blake",
	"Emilia", "Sloane", "Felicity", "Reese", "Phoenix",
	"Kinsley", "Adaline", "Journey", "Zara", "Remington",

	// Traditional Names (Various Origins)
	"Lucille", "Marion", "Francis", "Earl", "Gertrude",
	"Edith", "Mildred", "Ethel", "Bertha", "Alvin",
	"Walter", "Arthur", "Clyde", "Agnes", "Hilda",
	"Ruth", "Mabel", "Morris", "Arnold", "Pearl",
	"Josephine", "Winifred", "Milton", "Viola", "Alma",
	"Cora", "Elmer", "Harvey", "Sylvia", "Oscar",
	"Genevieve", "Fannie", "August", "Irving",
}

var lastNames = []string{
	// Common English Last Names
	"Smith", "Johnson", "Williams", "Brown", "Jones",
	"Garcia", "Miller", "Davis", "Rodriguez", "Martinez",
	"Hernandez", "Lopez", "Gonzalez", "Wilson", "Anderson",
	"Thomas", "Taylor", "Moore", "Jackson", "Martin",
	"Lee", "Perez", "Thompson", "White", "Harris",
	"Sanchez", "Clark", "Ramirez", "Lewis", "Robinson",
	"Walker", "Young", "Allen", "King", "Wright",
	"Scott", "Torres", "Nguyen", "Hill", "Flores",
	"Green", "Adams", "Nelson", "Baker", "Hall",
	"Rivera", "Campbell", "Mitchell", "Carter", "Roberts",
	"Gomez", "Phillips", "Evans", "Turner", "Diaz",
	"Parker", "Cruz", "Edwards", "Collins", "Reyes",
	"Stewart", "Morris", "Morales", "Murphy", "Cook",
	"Rogers", "Gutierrez", "Ortiz", "Morgan", "Cooper",
	"Peterson", "Bailey", "Reed", "Kelly", "Howard",
	"Ramos", "Kim", "Cox", "Ward", "Richardson",
	"Watson", "Brooks", "Chavez", "Wood", "James",
	"Bennett", "Gray", "Mendoza", "Ruiz", "Hughes",
	"Price", "Alvarez", "Castillo", "Sanders", "Patel",
	"Myers", "Long", "Ross", "Foster", "Jimenez",
	"Powell", "Jenkins", "Perry", "Russell", "Sullivan",

	// More Common Last Names
	"Bell", "Coleman", "Butler", "Henderson", "Barnes",
	"Gonzales", "Fisher", "Vasquez", "Simmons", "Romero",
	"Jordan", "Patterson", "Alexander", "Hamilton", "Graham",
	"Reynolds", "Griffin", "Wallace", "Moreno", "West",
	"Cole", "Hayes", "Bryant", "Herrera", "Gibson",
	"Ellis", "Tran", "Medina", "Aguilar", "Stevens",
	"Murray", "Ford", "Castro", "Marshall", "Owens",
	"Harrison", "Fernandez", "Mcdonald", "Woods", "Washington",
	"Kennedy", "Wells", "Vargas", "Henry", "Chen",
	"Freeman", "Webb", "Tucker", "Guzman", "Burns",
	"Crawford", "Olson", "Simpson", "Porter", "Hunter",
	"Gordon", "Mendez", "Silva", "Shaw", "Snyder",
	"Mason", "Dixon", "Muñoz", "Hunt", "Hicks",
	"Holmes", "Palmer", "Wagner", "Black", "Robertson",
	"Boyd", "Rose", "Stone", "Salazar", "Fox",
	"Warren", "Mills", "Meyer", "Rice", "Schmidt",
	"Garza", "Daniels", "Ferguson", "Nichols", "Stephens",
	"Soto", "Weaver", "Ryan", "Gardner", "Payne",

	// Additional Common Last Names
	"Grant", "Dunn", "Kelley", "Spencer", "Hawkins",
	"Arnold", "Pierce", "Vasquez", "Hansen", "Peters",
	"Santos", "Hart", "Bradley", "Knight", "Elliott",
	"Cunningham", "Duncan", "Armstrong", "Hudson", "Carroll",
	"Lane", "Riley", "Andrews", "Alvarado", "Ray",
	"Delgado", "Berry", "Perkins", "Hoffman", "Johnston",
	"Matthews", "Peña", "Richards", "Contreras", "Willis",
	"Carpenter", "Lawrence", "Sandoval", "Guerrero", "George",
	"Chapman", "Rios", "Estrada", "Ortega", "Watkins",
	"Greene", "Nunez", "Wheeler", "Valdez", "Harper",
	"Burke", "Larson", "Soto", "Bishop", "Burnett",
	"Hansen", "Rice", "Pena", "Schmidt", "Richards",
	"Willis", "Lawson", "Watts", "Little", "Swanson",
	"Day", "Mejia", "Fowler", "Chapman", "Love",
	"Jacobs", "Duarte", "Gross", "Arias", "Fleming",
	"Mendoza", "O'Neill", "Todd", "Bates", "Hodges",
	"Aguirre", "Montoya", "Reese", "Ellison", "Wilkinson",
	"Nash", "McClure", "Stokes", "Kemp", "Wilkins",
	"Serrano", "Frederick", "Hurst", "Deleon", "Briggs",

	// Hispanic Surnames
	"Alonso", "Cortez", "Esparza", "Maldonado", "Ibarra",
	"Nieves", "Fuentes", "Pacheco", "Arellano", "Guzmán",
	"Valenzuela", "Navarro", "Rios", "Campos", "Miranda",
	"Benitez", "Zamora", "Vega", "Molina", "Solis",
	"Medrano", "Gallegos", "Meza", "Montes", "Salinas",
	"Avila", "Delgado", "Acosta", "Cano", "Espinoza",
	"Villalobos", "Delacruz", "Lugo", "Luna", "Tapia",
	"Rojas", "Sosa", "Villanueva", "Beltran", "Bustamante",
	"Paredes", "Cervantes", "Acevedo", "Roman", "Figueroa",
	"Quintero", "Olivas", "Salcedo", "Amador", "Almanza",

	// French Last Names
	"Dubois", "Lefevre", "Moreau", "Simon", "Laurent",
	"Lemoine", "Martineau", "Poirier", "Renard", "Fontaine",
	"Lambert", "Dufresne", "Perrault", "Marchand", "Dufour",
	"Morel", "Chevalier", "Fournier", "Dupont", "Leclerc",
	"Bouchard", "Blanchard", "Gagnon", "Bertrand", "Girard",
	"Gaillard", "Masson", "Robin", "Descoteaux", "Duval",
	"Garnier", "Lacombe", "Armand", "Beaumont", "Valois",
	"Baudelaire", "Charbonneau", "Carpentier", "Boucher", "Faure",
	"Mercier", "Rousseau", "Normand", "Tremblay", "Michaud",

	// Asian Last Names
	"Watanabe", "Yamamoto", "Kimura", "Suzuki", "Sato",
	"Takahashi", "Tanaka", "Nakamura", "Kobayashi", "Matsumoto",
	"Nguyen", "Tran", "Pham", "Le", "Vo",
	"Hoang", "Truong", "Dang", "Huynh", "Bui",
	"Choi", "Jeong", "Yoon", "Shin", "Hong",
	"Wu", "Chen", "Li", "Zhao", "Lin",
	"Yang", "Huang", "Zhu", "Tang", "Zhou",
	"Tsai", "Chiang", "Feng", "Wang", "Cheng",

	// European Last Names
	"Müller", "Schmidt", "Fischer", "Weber", "Becker",
	"Schulz", "Hoffmann", "Mayer", "Klein", "Koch",
	"Zimmermann", "Jensen", "Christensen", "Svendsen", "Pedersen",
	"Andersen", "Olsen", "Larsen", "Sørensen", "Mikkelsen",
	"Johansson", "Nilsson", "Larsson", "Olsson", "Persson",
	"Karlsson", "Svensson", "Eriksson", "Nilssen", "Gustafsson",
	"Lopez", "Costa", "Martinez", "Martin", "Fernandez",
}

type Mocker struct {
	lastUID uint32
}

func New() *Mocker {
	return &Mocker{
		lastUID: uint32(rand.Int31n(1000)),
	}
}

func (m *Mocker) RandomMessage() *models.Message {
	m.lastUID += 1

	subject := randomString(emailSubjects...)
	if rand.Intn(5) == 0 {
		subject = "Re: " + subject
		if rand.Intn(2) == 0 {
			subject = "Re: " + subject
		}
	}

	middlename := ""
	if rand.Intn(20) == 0 {
		middlename = randomString(firstNames...)[0:1] + ". "
	}

	return &models.Message{
		SequenceID: m.lastUID,
		ID:         models.MessageID(m.lastUID),
		Sent:       time.Now(),
		Sender:     randomString(firstNames...) + " " + middlename + randomString(lastNames...),
		Subject:    subject,
		Size:       rand.Intn(7203680) + 200,
		Status:     models.StatusNew,
		UID:        m.lastUID,
	}
}

func (m *Mocker) OldRandomMessage() *models.Message {
	msg := m.RandomMessage()
	msg.Sent = time.Now().Add(-time.Hour*time.Duration(rand.Intn(1000)) - time.Duration(rand.Intn(60))*time.Minute)
	msg.Starred = rand.Intn(10) == 0
	msg.Answered = rand.Intn(7) == 0
	msg.Forwarded = rand.Intn(25) == 0
	msg.Status = models.StatusRead
	if rand.Intn(20) == 0 {
		msg.Status = models.StatusNew
		msg.Starred = false
		msg.Answered = false
		msg.Forwarded = false
	}
	return msg
}

//go:embed welcome-msg.html
var welcomeIntro []byte

var welcomeMail []byte

func init() {
	tpl, err := template.New("welcome").Parse(string(welcomeIntro))
	if err != nil {
		panic(err)
	}

	name := "ELMA User"
	if u, err := user.Current(); err == nil && u != nil && u.Name != "" {
		name = u.Name
	}

	buf := bytes.Buffer{}
	if err := tpl.Execute(&buf, map[string]string{
		"name": name,
	}); err != nil {
		panic(err)
	}

	docs, err := docs.ByteSlices()
	if err != nil {
		panic(err)
	}

	for _, doc := range docs {
		if _, err := buf.Write(doc); err != nil {
			panic(err)
		}
	}

	welcomeMail = buf.Bytes()
}

func MessageContent() *models.MessageContent {
	return &models.MessageContent{
		Parts: []models.MessageContentPart{
			{
				ContentType: "text/html",
				Content:     welcomeMail,
			},
		},
	}
}
